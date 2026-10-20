use crate::child_reaper::ReapableChild;
use crate::conmon_capnp::conmon;
use anyhow::{Context, Result, bail};
use capnp::enum_list::Reader;
use libc::pid_t;
use nix::{
    errno::Errno,
    mount::{MsFlags, mount, umount},
    sched::{CloneFlags, unshare},
    sys::{
        prctl,
        signal::{Signal, kill},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::{ForkResult, Gid, Pid, Uid, fork, getpid, setresgid, setresuid},
};
use signal_hook::{
    consts::{SIGCHLD, TERM_SIGNALS},
    iterator::Signals,
};
use std::{
    env, fmt,
    fs::{self, File},
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};
use tokio::task;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, info, trace, warn};

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

    /// The PID of the long running pause process. It only exists if a PID
    /// namespace was requested, because that is the only namespace which
    /// dies with its init process. All other namespaces stay alive through
    /// their bind mounts alone.
    pid: Option<Pid>,
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
    pub fn pid(&self) -> Option<Pid> {
        self.pid
    }
}

/// The global shared multiple pause instance.
static PAUSE: OnceLock<Pause> = OnceLock::new();

impl Pause {
    /// Retrieve the global instance of pause, creating it on the first call.
    ///
    /// A server holds the namespaces of a single pod. Later calls must ask for
    /// the same pod and configuration, they would get other namespaces than
    /// requested otherwise.
    pub fn init_shared(
        base_path: &str,
        pod_id: &str,
        namespaces: Reader<conmon::Namespace>,
        uid_mappings: Vec<String>,
        gid_mappings: Vec<String>,
    ) -> Result<&'static Pause> {
        let base_path = PathBuf::from(base_path);
        let namespaces = Self::parse_namespaces(namespaces)?;
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

        if let Some(pause) = PAUSE.get() {
            pause
                .ensure_matches(
                    &base_path,
                    pod_id,
                    &namespaces,
                    &uid_mappings,
                    &gid_mappings,
                )
                .context("pause already initialized")?;
            return Ok(pause);
        }

        let pause = Self::init(base_path, pod_id, namespaces, uid_mappings, gid_mappings)
            .context("init pause")?;
        if let Some(pid) = pause.pid() {
            Self::spawn_reaper(pid);
        }
        let _ = PAUSE.set(pause);
        PAUSE.get().context("pause not initialized")
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
    fn parse_namespaces(requested: Reader<conmon::Namespace>) -> Result<Vec<Namespace>> {
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
    fn spawn_reaper(pid: Pid) {
        task::spawn(
            async move {
                let code = ReapableChild::wait_for_exit_code_async(
                    CancellationToken::new(),
                    pid.as_raw() as u32,
                )
                .await;
                info!("Pause PID {pid} exited with code {code}");
            }
            .instrument(debug_span!("pause_reaper")),
        );
    }

    /// Retrieve the global instance of pause if initialized.
    pub fn maybe_shared() -> Option<&'static Pause> {
        PAUSE.get()
    }

    /// Stop the global pause instance.
    pub fn stop(&self) {
        info!("Stopping pause");
        for namespace in self.namespaces() {
            if let Err(e) = namespace.umount(self.base_path(), self.pod_id()) {
                debug!("Unable to umount namespace {namespace}: {:#}", e);
            }
        }

        if let Some(pid) = self.pid() {
            info!("Killing pause PID: {pid}");
            if let Err(e) = kill(pid, Signal::SIGTERM) {
                warn!("Unable to kill pause PID {pid}: {:#}", e);
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
    fn init(
        base_path: PathBuf,
        pod_id: &str,
        namespaces: Vec<Namespace>,
        uid_mappings: Vec<String>,
        gid_mappings: Vec<String>,
    ) -> Result<Self> {
        debug!("Initializing pause");

        let mut args: Vec<String> = vec![format!("--pod-id={pod_id}")];
        for mapping in &uid_mappings {
            args.push(format!("--uid-mappings={mapping}"));
        }
        for mapping in &gid_mappings {
            args.push(format!("--gid-mappings={mapping}"));
        }
        for namespace in &namespaces {
            args.push(format!("--{namespace}"));
        }

        debug!("Pause namespaces: {:?}", namespaces);
        debug!("Pause args: {:?}", args);

        fs::create_dir_all(&base_path).context("create base path")?;
        debug!("Pause base path: {}", base_path.display());

        let program = env::args().next().context("no args set")?;
        let mut child = Command::new(program)
            .arg("pause")
            .arg("--base-path")
            .arg(&base_path)
            .args(args)
            // The init of a PID namespace outlives this call. It must not keep
            // the output of the server open, only errors are of interest.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .context("run pause")?;

        let status = child.wait().context("wait for pause child")?;
        if !status.success() {
            bail!("exit status not ok: {status}")
        }

        // Without a PID namespace the pause process already exited after
        // binding the namespaces and left no PID behind.
        let pid = if namespaces.contains(&Namespace::Pid) {
            let pid = fs::read_to_string(Self::pause_pid_path(&base_path, pod_id))
                .context("read pause PID path")?
                .trim()
                .parse::<u32>()
                .context("parse pause PID")?;
            info!("Pause PID is: {pid}");
            Some(Pid::from_raw(pid as pid_t))
        } else {
            info!("No PID namespace requested, pause process exited after binding");
            None
        };

        Ok(Self {
            base_path,
            pod_id: pod_id.to_owned().into_boxed_str(),
            namespaces,
            uid_mappings,
            gid_mappings,
            pid,
        })
    }

    /// Retrieve the pause PID path for a base and pod ID.
    fn pause_pid_path<T: AsRef<Path>>(base_path: T, pod_id: &str) -> PathBuf {
        let mut path = base_path.as_ref().join("conmonrs");
        path.push(pod_id);
        path.set_extension("pid");
        path
    }

    #[allow(clippy::too_many_arguments)]
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
    pub fn run<T: AsRef<Path> + Copy>(
        base_path: T,
        pod_id: &str,
        ipc: bool,
        pid: bool,
        net: bool,
        user: bool,
        uts: bool,
        uid_mappings: &[String],
        gid_mappings: &[String],
    ) -> Result<()> {
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
                    base_path: base_path.as_ref(),
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
            match unsafe { fork().context("forking PID namespace init")? } {
                ForkResult::Parent { child } => child,
                ForkResult::Child => {
                    drop(sock);
                    return Self::run_init();
                }
            }
        } else {
            getpid()
        };

        // Report the process holding all namespaces
        sock.write_all(&holder.as_raw().to_ne_bytes())
            .context("report namespace holder PID")?;

        // Wait for the parent to bind them
        sock.read_exact(&mut res).context("wait for mounts")?;

        // The bind mounts keep the namespaces alive on their own, only a PID
        // namespace dies with its init, which stays around.
        debug!("Namespaces bound, exiting pause child");
        Ok(())
    }

    /// The init of the PID namespace, which keeps it alive until terminated.
    /// Processes of the pod which lose their parent get reparented to it, so it
    /// reaps them like any init.
    fn run_init() -> Result<()> {
        let mut signals = Signals::new(TERM_SIGNALS.iter().copied().chain([SIGCHLD]))
            .context("register signals")?;
        for signal in signals.forever() {
            if signal != SIGCHLD {
                debug!("Got signal {signal}, exiting pause init");
                return Ok(());
            }
            Self::reap_children();
        }
        bail!("signal iterator ended")
    }

    /// Reap all exited children without blocking.
    fn reap_children() {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => return,
                Ok(status) => trace!("Reaped child: {status:?}"),
                Err(Errno::EINTR) => continue,
                Err(e) => {
                    warn!("Unable to reap children: {:#}", e);
                    return;
                }
            }
        }
    }

    /// Write user or group ID mappings.
    fn write_mappings(mappings: &[String], pid: Pid, is_group: bool) -> Result<()> {
        let mut path = PathBuf::from("/proc");
        path.push(pid.as_raw().to_string());
        path.push(if is_group { "gid_map" } else { "uid_map" });

        let mut file = File::options()
            .write(true)
            .open(path)
            .context("open mapping file")?;

        for mapping in mappings {
            // Validate the mapping
            let mut split = mapping.split_whitespace();
            if split.clone().count() != 3 {
                bail!("mapping '{mapping}' has wrong format, expected 'CONTAINER_ID HOST_ID SIZE'");
            }
            if !split.all(|x| x.parse::<u32>().is_ok()) {
                bail!("mapping '{mapping}' has wrong format, expected all to be u32");
            }

            file.write_all(format!("{mapping}\n").as_bytes())
                .context("write mapping")?;
        }

        Ok(())
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
            warn!("Unable to wait for pause child {child}: {:#}", e);
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
            debug!("Unable to kill pause init {init}: {:#}", e);
        }

        // Do not leave the child blocked on the socket or as a zombie.
        if let Err(e) = kill(child, Signal::SIGKILL) {
            debug!("Unable to kill pause child {child}: {:#}", e);
        }
        if let Err(e) = waitpid(child, None) {
            debug!("Unable to wait for pause child {child}: {:#}", e);
        }

        // With the child gone, the killed init got reparented to this process,
        // the subreaper.
        if let Some(init) = init
            && let Err(e) = waitpid(init, None)
        {
            debug!("Unable to wait for pause init {init}: {:#}", e);
        }

        for namespace in self.namespaces {
            if let Err(e) = namespace.umount(self.base_path, self.pod_id) {
                debug!("Unable to umount namespace {namespace}: {:#}", e);
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
            trace!("Unable to umount namespace {self}: {:#}", e);
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

    #[test]
    fn ensure_matches_config() {
        let mappings = vec!["0 0 1".to_string()];
        let pause = Pause {
            base_path: "/var/run".into(),
            pod_id: "pod".into(),
            namespaces: vec![Namespace::Ipc, Namespace::User],
            uid_mappings: mappings.clone(),
            gid_mappings: mappings.clone(),
            pid: None,
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
}
