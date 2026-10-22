use crate::{
    child_reaper::ReapableChild,
    config::PauseArgs,
    conmon_capnp::conmon,
    mutex::lock,
    process::{Process, child_pid, read_pid_file},
};
use anyhow::{Context, Result, bail, format_err};
use capnp::enum_list::Reader;
use libc::pid_t;
use nix::{
    errno::Errno,
    mount::{MsFlags, mount, umount},
    sched::{CloneFlags, unshare},
    sys::{
        prctl,
        signal::{SigSet, Signal, kill},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::{ForkResult, Gid, Pid, SysconfVar, Uid, fork, getpid, setresgid, setresuid, sysconf},
};
use std::{
    env, fmt,
    fs::{self, File},
    io::{Read, Write},
    os::unix::{net::UnixStream, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    process::Command,
    sync::{OnceCell, watch},
    task,
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, debug, debug_span, info, trace, warn};

/// The main structure for this module.
#[derive(Debug)]
pub struct Pause {
    base_path: PathBuf,
    pod_id: Box<str>,

    /// The namespaces which were requested, created and bound.
    namespaces: Vec<Namespace>,

    /// The ID mappings of the user namespace, empty without one.
    uid_mappings: Vec<String>,
    gid_mappings: Vec<String>,

    /// The long running pause process. It only exists if a PID namespace
    /// was requested, because that is the only namespace which dies with its
    /// init process. All other namespaces stay alive through their bind
    /// mounts alone.
    process: Option<Process>,

    /// Set once the reaper took the exit of the pause process.
    exited: Arc<AtomicBool>,
}

impl Pause {
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }
    pub fn pod_id(&self) -> &str {
        &self.pod_id
    }
    pub fn namespaces(&self) -> &[Namespace] {
        &self.namespaces
    }
}

/// The pause instance of this server.
static PAUSE: SingleFlight<Pause> = SingleFlight::new();

impl Pause {
    /// Retrieve the global instance of pause, creating it on the first call.
    ///
    /// A server holds the namespaces of a single pod. Later calls must ask for
    /// the same pod and configuration, they would get other namespaces than
    /// requested otherwise.
    pub async fn init_shared(
        base_path: &str,
        pod_id: &str,
        namespaces: Vec<Namespace>,
        uid_mappings: Vec<String>,
        gid_mappings: Vec<String>,
    ) -> Result<&'static Pause> {
        let base_path = PathBuf::from(base_path);
        let (uid_mappings, gid_mappings) = if namespaces.contains(&Namespace::User) {
            if uid_mappings.is_empty() {
                bail!("user ID mappings are empty")
            }
            if gid_mappings.is_empty() {
                bail!("group ID mappings are empty")
            }
            (uid_mappings, gid_mappings)
        } else {
            // The mappings only apply to a user namespace.
            (vec![], vec![])
        };

        let pause = {
            let pod_id = pod_id.to_owned();
            let (base_path, namespaces, uid_mappings, gid_mappings) = (
                base_path.clone(),
                namespaces.clone(),
                uid_mappings.clone(),
                gid_mappings.clone(),
            );
            PAUSE
                .get_or_init(move || async move {
                    Self::init(
                        &base_path,
                        &pod_id,
                        &namespaces,
                        &uid_mappings,
                        &gid_mappings,
                    )
                    .await
                    .context("init pause")
                })
                .await?
        };

        // The instance may differ from the request if another one created it
        // first, which is also fine to check for the own one.
        pause
            .ensure_matches(
                &base_path,
                pod_id,
                &namespaces,
                &uid_mappings,
                &gid_mappings,
            )
            .context("pause already initialized")?;

        // The shutdown may have started meanwhile and stops the instance.
        if PAUSE.shutting_down() {
            bail!("server is shutting down")
        }

        // The PID namespace died with its init, the path would refer to a
        // dead namespace. A new one cannot replace it, the containers of the
        // pod which run already would end up in another PID namespace than
        // the later ones. So the pod has to be recreated.
        if !pause.is_alive() {
            bail!("the pause process of the PID namespace exited, recreate the pod")
        }
        Ok(pause)
    }

    /// Whether the pause process holding the PID namespace still runs, true
    /// without one. An init which exited but was not reaped yet does not.
    fn is_alive(&self) -> bool {
        self.process
            .as_ref()
            .is_none_or(|process| !self.exited.load(Ordering::SeqCst) && process.is_running())
    }

    /// How long before the deadline of the shutdown it kills the pause
    /// subcommand of a running initialization, and how long it leaves for
    /// the unmounts of the pause instance, see [`Pause::shutdown`].
    pub const KILL_WAIT: Duration = Duration::from_secs(1);
    pub const STOP_WAIT: Duration = Duration::from_secs(1);

    /// Stop the pause instance for the shutdown, until the `deadline`. An
    /// initialization still running finishes in its own task, wait for it
    /// rather than leaving its pause process behind, and kill its subcommand
    /// if it does not finish. No request initializes one after this. It fails
    /// if the initialization or the stop did not finish in time.
    pub async fn shutdown(deadline: Instant) -> Result<()> {
        PAUSE
            .shutdown(deadline, Self::KILL_WAIT, Self::STOP_WAIT)
            .await
    }

    /// Fail if the pause instance was created for another pod or configuration.
    fn ensure_matches(
        &self,
        base_path: &Path,
        pod_id: &str,
        namespaces: &[Namespace],
        uid_mappings: &[String],
        gid_mappings: &[String],
    ) -> Result<()> {
        if self.pod_id() != pod_id {
            bail!(
                "created for pod {}, cannot serve pod {pod_id}",
                self.pod_id()
            );
        }
        if self.base_path() != base_path {
            bail!(
                "created with base path {}, not {}",
                self.base_path().display(),
                base_path.display()
            );
        }
        // The order does not matter, both lists are free of duplicates.
        if self.namespaces.len() != namespaces.len()
            || !namespaces.iter().all(|ns| self.namespaces.contains(ns))
        {
            bail!(
                "created with namespaces {:?}, not {:?}",
                self.namespaces,
                namespaces
            );
        }
        if self.uid_mappings != uid_mappings || self.gid_mappings != gid_mappings {
            bail!("created with other ID mappings")
        }
        Ok(())
    }

    /// Convert the requested namespaces, dropping duplicates.
    pub fn parse_namespaces(requested: Reader<conmon::Namespace>) -> Result<Vec<Namespace>> {
        let mut namespaces = vec![];
        for namespace in requested.iter() {
            let namespace = match namespace? {
                conmon::Namespace::Ipc => Namespace::Ipc,
                conmon::Namespace::Net => Namespace::Net,
                conmon::Namespace::Pid => Namespace::Pid,
                conmon::Namespace::Uts => Namespace::Uts,
                conmon::Namespace::User => Namespace::User,
            };

            // A namespace requested twice is created and reported once.
            if namespaces.contains(&namespace) {
                debug!("Ignoring duplicate namespace {namespace}");
                continue;
            }
            namespaces.push(namespace);
        }
        Ok(namespaces)
    }

    /// Reap the init of the PID namespace once it exits, it is a child of the
    /// server, which is the child subreaper. Its PID is known, so waiting for
    /// it does not take the exit status of any other child.
    fn spawn_reaper(pid: Pid, exited: Arc<AtomicBool>) {
        task::spawn(
            async move {
                let code =
                    ReapableChild::wait_for_exit_code_async(CancellationToken::new(), pid).await;
                exited.store(true, Ordering::SeqCst);
                if PAUSE.shutting_down() {
                    info!("Pause PID {pid} exited with code {code}");
                } else {
                    warn!(
                        "Pause PID {pid} exited unexpectedly with code {code}, the PID namespace is gone"
                    );
                }
            }
            .instrument(debug_span!("pause_reaper")),
        );
    }

    /// Stop the pause instance. Unmounting waits for the kernel, so it runs
    /// on the blocking pool, see [`stop_blocking`].
    fn stop(&self) {
        info!("Stopping pause");
        for namespace in self.namespaces() {
            if let Err(e) = namespace.umount(self.base_path(), self.pod_id()) {
                debug!("Unable to umount namespace {namespace}: {e:#}");
            }
        }

        if let Some(process) = &self.process {
            let pid = process.pid();
            info!("Killing pause PID: {pid}");
            match process.signal(Signal::SIGTERM) {
                Ok(()) => {}
                Err(Errno::ESRCH | Errno::ECHILD) => debug!("Pause PID {pid} already exited"),
                Err(e) => warn!("Unable to kill pause PID {pid}: {e:#}"),
            }
        }

        let pause_pid_path = Self::pause_pid_path(self.base_path(), self.pod_id());
        if let Err(e) = fs::remove_file(&pause_pid_path) {
            debug!(
                "Unable to remove pause PID path {}: {:#}",
                pause_pid_path.display(),
                e
            );
        }
    }

    /// Initialize a new pause instance.
    async fn init(
        base_path: &Path,
        pod_id: &str,
        namespaces: &[Namespace],
        uid_mappings: &[String],
        gid_mappings: &[String],
    ) -> Result<Self> {
        debug!("Initializing pause");

        let mut args: Vec<String> = vec![format!("--pod-id={pod_id}")];
        for mapping in uid_mappings {
            args.push(format!("--uid-mappings={mapping}"));
        }
        for mapping in gid_mappings {
            args.push(format!("--gid-mappings={mapping}"));
        }
        for namespace in namespaces {
            args.push(format!("--{namespace}"));
        }

        debug!("Pause namespaces: {:?}", namespaces);
        debug!("Pause args: {:?}", args);

        fs::create_dir_all(base_path).context("create base path")?;
        debug!("Pause base path: {}", base_path.display());

        // The running binary, also after a package upgrade replaced the file
        // current_exe would point at. The name stays the one of this process,
        // it would show up as "exe" otherwise.
        let arg0 = env::args_os()
            .next()
            .unwrap_or_else(|| env!("CARGO_PKG_NAME").into());
        // Unlike the runtimes it keeps the OOM score adjustment of the
        // server: the init of a PID namespace takes every process of the pod
        // with it.
        let mut child = Command::new("/proc/self/exe")
            .arg0(arg0)
            .arg("pause")
            .arg("--base-path")
            .arg(base_path)
            .args(args)
            // The init of a PID namespace outlives this call. It must not keep
            // the output of the server open, only errors are of interest.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Do not leave it behind if the runtime drops this task on the
            // shutdown. Its child then stops the init of a PID namespace.
            .kill_on_drop(true)
            .spawn()
            .context("run pause")?;

        // The shutdown kills it if it takes too long. It is not reaped before
        // the wait below, so the PID is still its own here. Dropping it on a
        // failure kills it.
        let process = Process::open(child_pid(child.id()).context("PID of pause child")?);
        PAUSE.set_child(Some(process))?;
        let status = child.wait().await;
        PAUSE.set_child(None)?;

        let mut pause = Self {
            base_path: base_path.to_path_buf(),
            pod_id: pod_id.into(),
            namespaces: namespaces.to_vec(),
            uid_mappings: uid_mappings.to_vec(),
            gid_mappings: gid_mappings.to_vec(),
            process: None,
            exited: Arc::default(),
        };

        let status = status.context("wait for pause child")?;
        if !status.success() {
            // The pause subcommand undoes its own failed setup, but not if it
            // got killed by the shutdown. Its child stops the init of a PID
            // namespace then, the bind mounts and PID file stay.
            if status.signal().is_some() {
                stop_blocking(move || pause.stop()).await;
            }
            bail!("exit status not ok: {status}")
        }

        // Without a PID namespace the pause process already exited after
        // binding the namespaces and left no PID behind.
        if namespaces.contains(&Namespace::Pid) {
            // The namespaces are bound, do not leave them behind. The init
            // cannot be stopped without its PID though.
            let pid = match Self::read_pause_pid(base_path, pod_id) {
                Ok(pid) => pid,
                Err(e) => {
                    warn!("Unable to read the pause PID, its init process stays behind");
                    stop_blocking(move || pause.stop()).await;
                    return Err(e);
                }
            };
            info!("Pause PID is: {pid}");
            // Before the reaper may take the exit status, the PID is only
            // reliable until then.
            pause.process = Some(Process::open(pid));
            Self::spawn_reaper(pid, pause.exited.clone());
        } else {
            info!("No PID namespace requested, pause process exited after binding");
        }

        Ok(pause)
    }

    /// Read the PID of the init which the pause process left behind.
    fn read_pause_pid(base_path: &Path, pod_id: &str) -> Result<Pid> {
        read_pid_file(&Self::pause_pid_path(base_path, pod_id)).context("read pause PID")
    }

    /// Retrieve the pause PID path for a base and pod ID.
    fn pause_pid_path<T: AsRef<Path>>(base_path: T, pod_id: &str) -> PathBuf {
        let mut path = base_path.as_ref().join("conmonrs");
        path.push(pod_id);
        path.set_extension("pid");
        path
    }

    /// Run a new pause instance.
    ///
    /// The process forks. The child unshares the requested namespaces and the
    /// parent, which stays in the host namespaces, binds them from the
    /// `/proc/<pid>/ns` entries of the process holding them. Binding from the
    /// child would not work for a user namespace: its mounts live in a mount
    /// namespace owned by the less privileged user namespace and never reach
    /// the host.
    ///
    /// The bind mounts keep every namespace alive except a PID namespace, which
    /// dies with its init process. Unsharing a PID namespace does not move the
    /// caller into it, so the child forks that init, which is the only process
    /// that stays around.
    #[expect(unsafe_code, reason = "forking")]
    pub fn run(args: &PauseArgs) -> Result<()> {
        let &PauseArgs {
            ref base_path,
            ref pod_id,
            ipc,
            pid,
            net,
            user,
            uts,
            ref uid_mappings,
            ref gid_mappings,
        } = args;
        let requested = [
            (ipc, Namespace::Ipc),
            (pid, Namespace::Pid),
            (net, Namespace::Net),
            (user, Namespace::User),
            (uts, Namespace::Uts),
        ];
        let namespaces: Vec<Namespace> = requested
            .into_iter()
            .filter_map(|(enabled, namespace)| enabled.then_some(namespace))
            .collect();

        let mut flags = CloneFlags::empty();
        if ipc {
            flags.insert(CloneFlags::CLONE_NEWIPC);
        }
        if pid {
            flags.insert(CloneFlags::CLONE_NEWPID);
        }
        if net {
            flags.insert(CloneFlags::CLONE_NEWNET);
        }
        if uts {
            flags.insert(CloneFlags::CLONE_NEWUTS);
        }

        // Become the subreaper of the init, so that a failed setup can reap
        // it after killing it. On success it gets reparented to the server once
        // this process exits.
        prctl::set_child_subreaper(true).context("set child subreaper")?;

        let (sock_parent, sock_child) = UnixStream::pair().context("create unix socket pair")?;

        // SAFETY: the pause process is single threaded at this point and the
        // child only unshares namespaces, talks over the socket pair and waits
        // for a signal, all of which is fine after a fork.
        match unsafe { fork().context("forking process")? } {
            ForkResult::Parent { child } => {
                // Close the child end, so that reads fail if the child dies.
                drop(sock_child);

                let mut parent = PauseParent {
                    base_path,
                    pod_id,
                    child,
                    namespaces: &namespaces,
                    holder: None,
                };
                let result = parent.run(uid_mappings, gid_mappings, sock_parent);
                if result.is_err() {
                    parent.cleanup();
                }
                result
            }
            ForkResult::Child => {
                drop(sock_parent);
                Self::run_child(flags, user, pid, sock_child)
            }
        }
    }

    /// The message exchanged between the pause parent and child.
    const MSG: &'static [u8] = &[1];

    /// The child side of the pause process: unshares the namespaces, forks the
    /// init of a PID namespace if requested, reports the process holding the
    /// namespaces and waits for the parent to bind them.
    #[expect(unsafe_code, reason = "forking the init of a PID namespace")]
    fn run_child(flags: CloneFlags, user: bool, pid: bool, mut sock: UnixStream) -> Result<()> {
        let mut res = [0];

        if user {
            unshare(CloneFlags::CLONE_NEWUSER).context("unshare into user namespace")?;

            // Notify that the user namespace is now created
            sock.write_all(Self::MSG)
                .context("notify about user namespace")?;

            // Wait for the mappings to be written
            sock.read_exact(&mut res).context("wait for mappings")?;

            // Set the UID and GID
            let uid = Uid::from_raw(0);
            setresuid(uid, uid, uid).context("set root uid")?;
            let gid = Gid::from_raw(0);
            setresgid(gid, gid, gid).context("set root gid")?;
        }

        // Unshare the rest of the namespaces after the user namespace, so that
        // it owns them.
        unshare(flags).context("unshare with clone flags")?;

        // Only the next child becomes part of a new PID namespace, as its init.
        let holder = if pid {
            // The init inherits the mask, so no signal arrives before it waits
            // for them: they stay pending until then.
            let signals = Self::init_signals();
            signals.thread_block().context("block init signals")?;
            // SAFETY: the child of the pause process is single threaded, the
            // init only waits for signals and reaps after the fork.
            match unsafe { fork().context("forking PID namespace init")? } {
                ForkResult::Parent { child } => {
                    signals.thread_unblock().context("unblock init signals")?;
                    child
                }
                ForkResult::Child => {
                    drop(sock);
                    return Self::run_init(signals);
                }
            }
        } else {
            getpid()
        };

        // Report the process holding all namespaces and wait for the parent
        // to bind them.
        let result = sock
            .write_all(&holder.as_raw().to_ne_bytes())
            .context("report namespace holder PID")
            .and_then(|()| sock.read_exact(&mut res).context("wait for mounts"));

        // The parent may be gone, for example killed by the server shutdown,
        // then nobody else stops the init of the PID namespace.
        if result.is_err()
            && pid
            && let Err(e) = kill(holder, Signal::SIGKILL)
        {
            debug!("Unable to kill pause init {holder}: {e:#}");
        }
        result?;

        // The bind mounts keep the namespaces alive on their own, only a PID
        // namespace dies with its init, which stays around.
        debug!("Namespaces bound, exiting pause child");
        Ok(())
    }

    /// The init of the PID namespace, which keeps it alive until terminated.
    /// Processes of the pod which lose their parent get reparented to it, so it
    /// reaps them like any init.
    ///
    /// The signals are blocked since the fork and taken with sigwait, so none
    /// of them gets lost. Several exits may be reported by a single SIGCHLD,
    /// every one reaps all exited children.
    fn run_init(signals: SigSet) -> Result<()> {
        loop {
            match signals.wait() {
                Ok(Signal::SIGCHLD) => Self::reap_children(),
                Ok(signal) => {
                    debug!("Got signal {signal}, exiting pause init");
                    return Ok(());
                }
                Err(Errno::EINTR) => continue,
                Err(e) => return Err(e).context("wait for signals"),
            }
        }
    }

    /// The signals the init waits for: the ones which terminate it and the
    /// exit of a child.
    fn init_signals() -> SigSet {
        let mut signals = SigSet::empty();
        for signal in [
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGCHLD,
        ] {
            signals.add(signal);
        }
        signals
    }

    /// Reap all exited children without blocking.
    fn reap_children() {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => return,
                Ok(status) => trace!("Reaped child: {status:?}"),
                Err(Errno::EINTR) => continue,
                Err(e) => {
                    warn!("Unable to reap children: {e:#}");
                    return;
                }
            }
        }
    }

    /// The most ranges a mapping file takes (`UID_GID_MAP_MAX_EXTENTS`),
    /// Linux before 4.15 takes only 5.
    const MAX_MAPPINGS: usize = 340;

    /// Write user or group ID mappings. The kernel takes all ranges with a
    /// single write only, at offset 0 and shorter than a page. Nothing gets
    /// written without any, the kernel rejects an empty write.
    fn write_mappings(mappings: &[String], pid: Pid, is_group: bool) -> Result<()> {
        if mappings.is_empty() {
            return Ok(());
        }
        let page_size = sysconf(SysconfVar::PAGE_SIZE)
            .ok()
            .flatten()
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(4096);
        let buf = Self::mappings_buffer(mappings, page_size)?;

        let mut path = PathBuf::from("/proc");
        path.push(pid.as_raw().to_string());
        path.push(if is_group { "gid_map" } else { "uid_map" });

        let mut file = File::options()
            .write(true)
            .open(path)
            .context("open mapping file")?;

        let written = file.write(buf.as_bytes()).context("write mappings")?;
        if written != buf.len() {
            bail!("wrote {written} of {} bytes of the mappings", buf.len())
        }
        Ok(())
    }

    /// Validate the mappings and build the content of a mapping file from
    /// them, one `CONTAINER_ID HOST_ID SIZE` line per range.
    fn mappings_buffer(mappings: &[String], page_size: usize) -> Result<String> {
        if mappings.len() > Self::MAX_MAPPINGS {
            bail!(
                "{} mappings exceed the limit of {}",
                mappings.len(),
                Self::MAX_MAPPINGS
            )
        }

        let mut buf = String::new();
        for mapping in mappings {
            let fields: Vec<&str> = mapping.split_whitespace().collect();
            if fields.len() != 3 {
                bail!("mapping '{mapping}' has wrong format, expected 'CONTAINER_ID HOST_ID SIZE'");
            }
            let mut ids = [0u32; 3];
            for (id, field) in ids.iter_mut().zip(fields) {
                *id = field.parse().with_context(|| {
                    format!("mapping '{mapping}' has wrong format, expected all to be u32")
                })?;
            }
            let [container_id, host_id, size] = ids;
            buf.push_str(&format!("{container_id} {host_id} {size}\n"));
        }

        if buf.len() >= page_size {
            bail!(
                "mappings take {} bytes, the kernel takes less than {page_size}",
                buf.len()
            )
        }
        Ok(buf)
    }
}

/// A value which has to be undone when it is no longer used.
trait Stop {
    /// Undo the value, which may block, see [`stop_blocking`].
    fn stop(&self);
}

/// Run the stop of a value on the blocking pool. Unmounting waits for the
/// kernel, which can take long, the runtime thread runs every RPC and the
/// I/O of all containers.
async fn stop_blocking(stop: impl FnOnce() + Send + 'static) {
    if let Err(e) = task::spawn_blocking(stop).await {
        warn!("Unable to stop: {e}");
    }
}

impl Stop for Pause {
    fn stop(&self) {
        Pause::stop(self);
    }
}

/// Creates a value of which a server has a single one, like the pause
/// instance, once and on demand.
///
/// Only one initialization runs at a time, requests which arrive meanwhile
/// wait for it instead of queueing their own. It runs in a task of its own,
/// so a request cancelled meanwhile, for example by a CRI-O timeout, cannot
/// abandon a half set up value. A failed or panicked initialization leaves
/// the cell empty, so the next request starts over.
struct SingleFlight<T> {
    cell: OnceCell<T>,

    /// Whether an initialization runs.
    running: LazyLock<watch::Sender<bool>>,

    /// The process of the running initialization, so that the shutdown can
    /// kill it if it does not finish in time.
    child: Mutex<Option<Process>>,

    /// Set once the server shuts down. No initialization starts after that
    /// and one which is still running stops its value itself, so that the
    /// shutdown does not leave one behind.
    shutting_down: AtomicBool,
}

/// Marks the initialization as finished, whatever happens to its task.
struct InitRunning<'a>(&'a watch::Sender<bool>);

impl Drop for InitRunning<'_> {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
}

impl<T: Stop + Send + Sync + 'static> SingleFlight<T> {
    const fn new() -> Self {
        Self {
            cell: OnceCell::const_new(),
            running: LazyLock::new(Self::not_running),
            child: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
        }
    }

    fn not_running() -> watch::Sender<bool> {
        watch::Sender::new(false)
    }

    fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Retrieve the value, running `init` if there is none and no other
    /// initialization runs. A request which waited for the initialization of
    /// another one fails if that failed, its outcome goes to the request
    /// which started it and a retry runs a new one.
    async fn get_or_init<F, Fut>(&'static self, init: F) -> Result<&'static T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send,
    {
        if self.shutting_down() {
            bail!("server is shutting down")
        }

        if self.cell.get().is_none() {
            // Claim the initialization if none runs.
            let claimed = self.running.send_if_modified(|running| {
                let claimed = !*running;
                *running = true;
                claimed
            });

            if claimed {
                let init = task::spawn(
                    async move {
                        let _running = InitRunning(&self.running);
                        self.cell
                            .get_or_try_init(|| self.init_unless_shutting_down(init))
                            .await
                            .map(drop)
                    }
                    .instrument(Span::current()),
                );
                init.await.context("join initialization")??;
            } else {
                self.wait_for_init().await;
            }
        }

        self.cell
            .get()
            .context("the initialization waited for failed, retry")
    }

    /// Wait until no initialization runs.
    async fn wait_for_init(&self) {
        // Only fails if the sender is gone, which lives as long as self.
        let _ = self.running.subscribe().wait_for(|running| !*running).await;
    }

    /// Run the initialization, unless the server shuts down. If the shutdown
    /// started meanwhile it may have given up waiting for this, so stop the
    /// value here instead of publishing it.
    async fn init_unless_shutting_down<F, Fut>(&self, init: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        if self.shutting_down() {
            bail!("server is shutting down")
        }
        let value = init().await?;
        if self.shutting_down() {
            stop_blocking(move || value.stop()).await;
            bail!("server is shutting down")
        }
        Ok(value)
    }

    /// Set the process of the running initialization.
    fn set_child(&self, child: Option<Process>) -> Result<()> {
        *lock(&self.child)? = child;
        Ok(())
    }

    /// Stop the value for the shutdown, until the `deadline`. Wait for a
    /// running initialization until `kill_wait` and `stop_wait` before it,
    /// kill its process if it does not finish and wait until `stop_wait`
    /// before the deadline for it to clean up, then for the stop of the
    /// value. No request initializes one after this.
    async fn shutdown(
        &'static self,
        deadline: Instant,
        kill_wait: Duration,
        stop_wait: Duration,
    ) -> Result<()> {
        self.shutting_down.store(true, Ordering::SeqCst);
        let stop_deadline = deadline.checked_sub(stop_wait).unwrap_or(deadline);
        let kill_deadline = stop_deadline
            .checked_sub(kill_wait)
            .unwrap_or(stop_deadline);

        let mut result = Ok(());
        if time::timeout_at(kill_deadline, self.wait_for_init())
            .await
            .is_err()
        {
            warn!("Initialization still running, killing its process");
            match lock(&self.child) {
                Ok(child) => {
                    if let Some(child) = &*child
                        && let Err(e) = child.signal(Signal::SIGKILL)
                    {
                        warn!(
                            "Unable to kill initialization process {}: {:#}",
                            child.pid(),
                            e
                        );
                    }
                }
                Err(e) => warn!("Unable to lock initialization process PID: {e:#}"),
            }
            if time::timeout_at(stop_deadline, self.wait_for_init())
                .await
                .is_err()
            {
                result = Err(format_err!(
                    "initialization did not finish, its process may stay behind"
                ));
            }
        }

        // A stop which hangs, like an unmount on a hung file system, must
        // not keep the server from stopping.
        if let Some(value) = self.cell.get()
            && time::timeout_at(deadline, stop_blocking(move || value.stop()))
                .await
                .is_err()
        {
            result = Err(format_err!("stop did not finish in time"));
        }
        result
    }
}

/// The parent side of the pause process.
struct PauseParent<'a> {
    base_path: &'a Path,
    pod_id: &'a str,

    /// The forked pause child, which unshares the namespaces.
    child: Pid,

    /// The requested namespaces.
    namespaces: &'a [Namespace],

    /// The process holding the namespaces, once reported by the child. It is
    /// the init of the PID namespace if one was requested.
    holder: Option<Pid>,
}

impl PauseParent<'_> {
    /// Write the ID mappings, bind the namespaces of the holder process
    /// reported by the child and record the PID of the holder if it is the init
    /// of a PID namespace.
    fn run(
        &mut self,
        uid_mappings: &[String],
        gid_mappings: &[String],
        mut sock: UnixStream,
    ) -> Result<()> {
        let child = self.child;

        if self.namespaces.contains(&Namespace::User) {
            // Wait for user namespace creation
            let mut res = [0];
            sock.read_exact(&mut res)
                .context("wait for user namespace")?;

            // Write mappings
            Pause::write_mappings(gid_mappings, child, true).context("write gid maps")?;
            Pause::write_mappings(uid_mappings, child, false).context("write uid maps")?;

            // Notify that user mappings have been written
            sock.write_all(Pause::MSG)
                .context("notify about written mappings")?;
        }

        // Wait for the child to have all namespaces in place and to report
        // the process holding them.
        let mut buf = [0; size_of::<pid_t>()];
        sock.read_exact(&mut buf)
            .context("wait for namespace holder PID")?;
        let pid = Pid::from_raw(pid_t::from_ne_bytes(buf));
        self.holder = Some(pid);

        for namespace in self.namespaces {
            namespace
                .bind(self.base_path, self.pod_id, pid)
                .with_context(|| {
                    format!(
                        "bind namespace to path: {}",
                        namespace.path(self.base_path, self.pod_id).display(),
                    )
                })?;
        }

        if self.namespaces.contains(&Namespace::Pid) {
            let pause_pid_path = Pause::pause_pid_path(self.base_path, self.pod_id);
            fs::create_dir_all(
                pause_pid_path
                    .parent()
                    .context("no parent for pause PID path")?,
            )
            .context("create pause PID parent path")?;
            let mut file = File::create(pause_pid_path).context("create pause PID file")?;
            write!(file, "{pid}").context("write PID to pause file")?;
        }

        // Notify that all mounts are created
        sock.write_all(Pause::MSG).context("notify about mounts")?;

        // The child exits right after the notification. Reap it here so that
        // it does not linger as a zombie. The namespaces are already in place,
        // so a failure is no reason to fail the whole setup.
        if let Err(e) = waitpid(child, None) {
            warn!("Unable to wait for pause child {child}: {e:#}");
        }

        Ok(())
    }

    /// Undo a failed setup: kill the pause processes and remove the bind
    /// mounts and PID file which may already exist.
    fn cleanup(&self) {
        let child = self.child;

        let init = self.holder.filter(|holder| *holder != child);
        if let Some(init) = init
            && let Err(e) = kill(init, Signal::SIGKILL)
        {
            debug!("Unable to kill pause init {init}: {e:#}");
        }

        // Do not leave the child blocked on the socket or as a zombie.
        if let Err(e) = kill(child, Signal::SIGKILL) {
            debug!("Unable to kill pause child {child}: {e:#}");
        }
        if let Err(e) = waitpid(child, None) {
            debug!("Unable to wait for pause child {child}: {e:#}");
        }

        // With the child gone, the killed init got reparented to this process,
        // the subreaper.
        if let Some(init) = init
            && let Err(e) = waitpid(init, None)
        {
            debug!("Unable to wait for pause init {init}: {e:#}");
        }

        for namespace in self.namespaces {
            if let Err(e) = namespace.umount(self.base_path, self.pod_id) {
                debug!("Unable to umount namespace {namespace}: {e:#}");
            }
        }

        let pause_pid_path = Pause::pause_pid_path(self.base_path, self.pod_id);
        if let Err(e) = fs::remove_file(&pause_pid_path) {
            debug!(
                "Unable to remove pause PID path {}: {:#}",
                pause_pid_path.display(),
                e
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// All available linux namespaces.
pub enum Namespace {
    /// IPC namespace. This creates new namespace for System V IPC POSIX message queues and
    /// similar.
    Ipc,

    /// The PID namespace. The child process becomes PID 1.
    Pid,

    /// The network namespace. The namespace is empty and has no connectivity, even localhost
    /// network, unless some setup is done afterwards.
    Net,

    /// The user namespace, which allows to segregate the user ID.
    User,

    /// The UTS namespace, which allows to change hostname of the new container.
    Uts,
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Namespace {
    /// The name of the namespace as used in `/proc/<pid>/ns` and the flags.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ipc => "ipc",
            Self::Pid => "pid",
            Self::Net => "net",
            Self::User => "user",
            Self::Uts => "uts",
        }
    }

    /// Bind the namespace of the process `pid` to the provided base path and
    /// pod ID.
    pub fn bind<T: AsRef<Path>>(&self, path: T, pod_id: &str, pid: Pid) -> Result<()> {
        let bind_path = self.path(path, pod_id);
        fs::create_dir_all(
            bind_path
                .parent()
                .context("no parent namespace bind path")?,
        )
        .context("create namespace parent path")?;
        File::create(&bind_path).context("create namespace bind path")?;
        let source_path = PathBuf::from("/proc")
            .join(pid.as_raw().to_string())
            .join("ns")
            .join(self.as_str());

        mount(
            Some(&source_path),
            &bind_path,
            None::<&Path>,
            MsFlags::MS_BIND,
            None::<&[u8]>,
        )
        .context("mount namespace")?;

        Ok(())
    }

    /// Umount the namespace.
    pub fn umount<T: AsRef<Path>>(&self, path: T, pod_id: &str) -> Result<()> {
        let bind_path = self.path(path, pod_id);
        if let Err(e) = umount(&bind_path) {
            trace!("Unable to umount namespace {self}: {e:#}");
        }
        fs::remove_file(&bind_path).context("remove namespace bind path")
    }

    /// Retrieve the bind path of the namespace for the provided base path and pod ID.
    pub fn path<T: AsRef<Path>>(&self, path: T, pod_id: &str) -> PathBuf {
        path.as_ref().join(format!("{self}ns")).join(pod_id)
    }

    pub fn to_capnp_namespace(self) -> conmon::Namespace {
        match self {
            Namespace::Ipc => conmon::Namespace::Ipc,
            Namespace::Pid => conmon::Namespace::Pid,
            Namespace::Net => conmon::Namespace::Net,
            Namespace::User => conmon::Namespace::User,
            Namespace::Uts => conmon::Namespace::Uts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::Command as StdCommand,
        sync::{Arc, atomic::AtomicUsize},
    };
    use tokio::sync::Notify;

    /// A child which gets killed and reaped if a test fails before it did.
    struct Guarded(std::process::Child);

    impl Guarded {
        fn spawn(cmd: &mut StdCommand) -> Result<Self> {
            Ok(Self(cmd.spawn()?))
        }
    }

    impl Drop for Guarded {
        fn drop(&mut self) {
            // Both do nothing for a child which got waited for already.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A value which counts how often it got stopped.
    struct Value(Arc<AtomicUsize>);

    impl Stop for Value {
        fn stop(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn flight() -> &'static SingleFlight<Value> {
        Box::leak(Box::new(SingleFlight::new()))
    }

    /// An initialization which counts its runs and takes a second.
    fn slow_init(
        inits: &Arc<AtomicUsize>,
        stops: &Arc<AtomicUsize>,
    ) -> impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + 'static
    {
        let (inits, stops) = (inits.clone(), stops.clone());
        move || {
            Box::pin(async move {
                inits.fetch_add(1, Ordering::SeqCst);
                time::sleep(Duration::from_secs(1)).await;
                Ok(Value(stops))
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_init_once() -> Result<()> {
        let flight = flight();
        let (inits, stops) = (Arc::default(), Arc::default());

        let (a, b, c) = tokio::join!(
            flight.get_or_init(slow_init(&inits, &stops)),
            flight.get_or_init(slow_init(&inits, &stops)),
            flight.get_or_init(slow_init(&inits, &stops)),
        );
        let (a, b, c) = (a?, b?, c?);
        assert!(std::ptr::eq(a, b) && std::ptr::eq(b, c));
        assert_eq!(inits.load(Ordering::SeqCst), 1);

        // Later callers get the value right away.
        flight.get_or_init(slow_init(&inits, &stops)).await?;
        assert_eq!(inits.load(Ordering::SeqCst), 1);
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn failed_init_allows_retry() -> Result<()> {
        let flight = flight();
        let (inits, stops) = (Arc::default(), Arc::default());

        let failing = || async {
            time::sleep(Duration::from_secs(1)).await;
            bail!("init failed")
        };
        let (first, waiter) = tokio::join!(
            flight.get_or_init(failing),
            flight.get_or_init(slow_init(&inits, &stops)),
        );
        assert!(format!("{:#}", first.err().context("no error")?).contains("init failed"));
        // The waiter does not run its own, it gets the failure.
        assert!(waiter.is_err());
        assert_eq!(inits.load(Ordering::SeqCst), 0);

        flight.get_or_init(slow_init(&inits, &stops)).await?;
        assert_eq!(inits.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_caller_does_not_abandon_init() -> Result<()> {
        let flight = flight();
        let (inits, stops) = (Arc::default(), Arc::default());

        let cancelled = time::timeout(
            Duration::from_millis(100),
            flight.get_or_init(slow_init(&inits, &stops)),
        )
        .await;
        assert!(cancelled.is_err());

        // The initialization went on, the next caller waits for it.
        flight.get_or_init(slow_init(&inits, &stops)).await?;
        assert_eq!(inits.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_running_init() -> Result<()> {
        let flight = flight();
        let (inits, stops) = (Arc::default(), Arc::<AtomicUsize>::default());

        let caller = task::spawn(flight.get_or_init(slow_init(&inits, &stops)));
        while !*flight.running.borrow() {
            task::yield_now().await;
        }

        let start = time::Instant::now();
        flight
            .shutdown(
                start + Duration::from_secs(12),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await?;
        assert_eq!(start.elapsed(), Duration::from_secs(1));

        // It finished after the shutdown started, so it stopped its value
        // instead of publishing it.
        assert!(caller.await?.is_err());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(flight.cell.get().is_none());

        // Nothing starts after the shutdown.
        assert!(flight.get_or_init(slow_init(&inits, &stops)).await.is_err());
        assert_eq!(inits.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_gives_up_on_hanging_init() -> Result<()> {
        let flight = flight();
        let stops = Arc::<AtomicUsize>::default();
        let release = Arc::new(Notify::new());

        let caller = task::spawn(flight.get_or_init({
            let (stops, release) = (stops.clone(), release.clone());
            move || async move {
                release.notified().await;
                Ok(Value(stops))
            }
        }));
        while !*flight.running.borrow() {
            task::yield_now().await;
        }

        let start = time::Instant::now();
        let err = flight
            .shutdown(
                start + Duration::from_secs(12),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .err()
            .map(|e| e.to_string());
        assert_eq!(start.elapsed(), Duration::from_secs(11));
        assert_eq!(
            err.as_deref(),
            Some("initialization did not finish, its process may stay behind")
        );
        assert_eq!(stops.load(Ordering::SeqCst), 0);

        // Once it finishes late it stops its value itself.
        release.notify_one();
        assert!(caller.await?.is_err());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(flight.cell.get().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_kills_init_process() -> Result<()> {
        let flight = flight();
        let stops = Arc::<AtomicUsize>::default();

        let caller = task::spawn(flight.get_or_init({
            let stops = stops.clone();
            move || async move {
                let mut child = Command::new("sleep").arg("60").kill_on_drop(true).spawn()?;
                let pid = child_pid(child.id())?;
                flight.set_child(Some(Process::open(pid)))?;
                let status = child.wait().await?;
                flight.set_child(None)?;
                assert_eq!(status.signal(), Some(Signal::SIGKILL as i32));
                Ok(Value(stops))
            }
        }));
        while lock(&flight.child)?.is_none() {
            task::yield_now().await;
        }

        flight
            .shutdown(
                time::Instant::now() + Duration::from_millis(11100),
                Duration::from_secs(10),
                Duration::from_secs(1),
            )
            .await?;
        assert!(caller.await?.is_err());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_initialized_value() -> Result<()> {
        let flight = flight();
        let (inits, stops) = (Arc::default(), Arc::<AtomicUsize>::default());
        flight.get_or_init(slow_init(&inits, &stops)).await?;

        flight
            .shutdown(
                time::Instant::now() + Duration::from_secs(12),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await?;
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// A value whose stop hangs until released, like an unmount on a hung
    /// file system, and which tells the thread it stopped on.
    struct Hung {
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        stopped_on: Mutex<Option<std::thread::ThreadId>>,
    }

    impl Stop for Hung {
        fn stop(&self) {
            if let Ok(mut stopped_on) = self.stopped_on.lock() {
                *stopped_on = Some(std::thread::current().id());
            }
            if let Ok(release) = self.release.lock() {
                let _ = release.recv();
            }
        }
    }

    #[tokio::test]
    async fn shutdown_does_not_wait_for_a_hung_stop() -> Result<()> {
        let flight: &'static SingleFlight<Hung> = Box::leak(Box::new(SingleFlight::new()));
        let (release, rx) = std::sync::mpsc::channel();
        let value = flight
            .get_or_init(move || async move {
                Ok(Hung {
                    release: Mutex::new(rx),
                    stopped_on: Mutex::default(),
                })
            })
            .await?;

        // The runtime thread keeps running while the stop hangs.
        let ticker = task::spawn(async {
            for _ in 0..5 {
                time::sleep(Duration::from_millis(20)).await;
            }
        });
        let start = time::Instant::now();
        let res = flight
            .shutdown(
                start + Duration::from_millis(200),
                Duration::from_secs(1),
                Duration::from_millis(200),
            )
            .await;
        let elapsed = start.elapsed();
        assert_eq!(
            res.err().map(|e| e.to_string()).as_deref(),
            Some("stop did not finish in time")
        );
        assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert!(ticker.is_finished());
        let stopped_on = *lock(&value.stopped_on)?;
        assert!(stopped_on.is_some_and(|id| id != std::thread::current().id()));

        // Let the blocking pool end with the test.
        release.send(())?;
        Ok(())
    }

    #[test]
    fn process_signal_reaches_child() -> Result<()> {
        for use_pidfd in [true, false] {
            let mut child = Guarded::spawn(StdCommand::new("sleep").arg("60"))?;
            let pid = child_pid(child.0.id())?;
            let mut process = Process::open(pid);
            if !use_pidfd {
                process.pidfd = None;
            }

            process.signal(Signal::SIGTERM)?;
            assert_eq!(child.0.wait()?.signal(), Some(Signal::SIGTERM as i32));
        }
        Ok(())
    }

    #[test]
    fn process_signal_skips_reaped_child() -> Result<()> {
        let mut child = StdCommand::new("true").spawn()?;
        let pid = child_pid(child.id())?;
        let mut process = Process::open(pid);
        assert!(process.pidfd.is_some());
        child.wait()?;

        // The PID may belong to another process by now, neither way signals it.
        assert_eq!(process.signal(Signal::SIGTERM), Err(Errno::ESRCH));
        process.pidfd = None;
        assert_eq!(process.signal(Signal::SIGTERM), Err(Errno::ECHILD));
        Ok(())
    }

    #[test]
    fn init_signals() {
        let signals = Pause::init_signals();
        for signal in [
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGCHLD,
        ] {
            assert!(signals.contains(signal));
        }
        assert!(!signals.contains(Signal::SIGHUP));
    }

    #[test]
    fn dead_pause_process_is_noticed() -> Result<()> {
        let pause = |process| Pause {
            base_path: "/var/run".into(),
            pod_id: "pod".into(),
            namespaces: vec![Namespace::Pid],
            uid_mappings: vec![],
            gid_mappings: vec![],
            process,
            exited: Arc::default(),
        };
        assert!(pause(None).is_alive());

        let mut child = Guarded::spawn(StdCommand::new("sleep").arg("60"))?;
        let running = pause(Some(Process::open(child_pid(child.0.id())?)));
        assert!(running.is_alive());
        running.exited.store(true, Ordering::SeqCst);
        assert!(!running.is_alive());

        // Also before the reaper ran, as a zombie and once reaped.
        let running = pause(Some(Process::open(child_pid(child.0.id())?)));
        child.0.kill()?;
        let pid = nix::sys::wait::Id::Pid(child_pid(child.0.id())?);
        nix::sys::wait::waitid(pid, WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT)?;
        assert!(!running.is_alive());
        child.0.wait()?;
        assert!(!running.is_alive());
        Ok(())
    }

    #[test]
    fn ensure_matches_config() {
        let mappings = vec!["0 0 1".to_string()];
        let pause = Pause {
            base_path: "/var/run".into(),
            pod_id: "pod".into(),
            namespaces: vec![Namespace::Ipc, Namespace::User],
            uid_mappings: mappings.clone(),
            gid_mappings: mappings.clone(),
            process: None,
            exited: Arc::default(),
        };
        let namespaces = [Namespace::User, Namespace::Ipc];
        let base_path = Path::new("/var/run/");

        assert!(
            pause
                .ensure_matches(base_path, "pod", &namespaces, &mappings, &mappings)
                .is_ok()
        );
        assert!(
            pause
                .ensure_matches(base_path, "other", &namespaces, &mappings, &mappings)
                .is_err()
        );
        assert!(
            pause
                .ensure_matches(Path::new("/tmp"), "pod", &namespaces, &mappings, &mappings)
                .is_err()
        );
        assert!(
            pause
                .ensure_matches(base_path, "pod", &[Namespace::Ipc], &mappings, &mappings)
                .is_err()
        );
        assert!(
            pause
                .ensure_matches(base_path, "pod", &namespaces, &[], &mappings)
                .is_err()
        );
    }

    #[test]
    fn mappings_buffer() -> Result<()> {
        let mappings = |m: &[&str]| m.iter().map(ToString::to_string).collect::<Vec<_>>();

        // All ranges end up in one buffer, normalized to one line each.
        assert_eq!(
            Pause::mappings_buffer(&mappings(&["0 1000 1", "1\t100000  65536"]), 4096)?,
            "0 1000 1\n1 100000 65536\n"
        );
        assert_eq!(Pause::mappings_buffer(&[], 4096)?, "");
        // Nothing gets opened or written without mappings.
        Pause::write_mappings(&[], Pid::from_raw(pid_t::MAX), false)?;

        for invalid in [&["0 1000"][..], &["0 1000 1 2"], &["0 1000 x"], &["0 -1 1"]] {
            assert!(Pause::mappings_buffer(&mappings(invalid), 4096).is_err());
        }

        // A later invalid mapping fails before anything gets written.
        assert!(Pause::mappings_buffer(&mappings(&["0 0 1", "1 x 1"]), 4096).is_err());

        // The kernel takes less than a page and a limited number of ranges.
        let line = "4294967295 4294967295 1";
        let many = vec![line.to_string(); Pause::MAX_MAPPINGS];
        let err = Pause::mappings_buffer(&many, 4096).unwrap_err();
        assert!(format!("{err:#}").contains("less than 4096"), "{err:#}");
        assert!(Pause::mappings_buffer(&many, 65536).is_ok());
        let too_many = vec!["0 0 1".to_string(); Pause::MAX_MAPPINGS + 1];
        let err = Pause::mappings_buffer(&too_many, 65536).unwrap_err();
        assert!(format!("{err:#}").contains("limit of 340"), "{err:#}");
        Ok(())
    }
}
