//! The pause instance of a pod, a process which holds the namespaces the
//! pod shares, bind mounted below the base path so that they outlive it.

use crate::{
    child_reaper::{RuntimeOutput, with_stderr},
    conmon_capnp::conmon,
    process::{Process, child_pid, read_pid_file, wait_for_exit_code},
    single_flight::{SingleFlight, Stop, stop_blocking},
};
use anyhow::{Context, Result, bail};
use capnp::enum_list::Reader;
use nix::{
    errno::Errno,
    mount::{MsFlags, mount, umount},
    sys::signal::Signal,
    unistd::Pid,
};
use std::{
    env, fmt,
    fs::{self, File},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    process::Command,
    task,
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, info, trace, warn};

mod subprocess;

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
        Self::validate_location(&base_path, pod_id)?;
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

    /// The longest pod ID, whose PID file name still fits into `NAME_MAX`.
    const MAX_POD_ID_LEN: usize = 255 - ".pid".len();

    /// Fail unless the namespaces and the PID file of the pod end up below
    /// the base path. The pod ID names a file there, so it must be a single
    /// file name, and a relative base path would depend on the working
    /// directory of the server.
    fn validate_location(base_path: &Path, pod_id: &str) -> Result<()> {
        if !base_path.is_absolute() {
            bail!("base path {} is not absolute", base_path.display())
        }
        if pod_id.is_empty() {
            bail!("no pod ID provided")
        }
        if matches!(pod_id, "." | "..")
            || !pod_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        {
            bail!("pod ID {pod_id:?} has to consist of letters, digits, '_', '.' and '-' only")
        }
        // The PID file adds ".pid", a longer one would only fail once the
        // namespaces got bound.
        if pod_id.len() > Self::MAX_POD_ID_LEN {
            bail!(
                "pod ID is {} bytes long, the limit is {}",
                pod_id.len(),
                Self::MAX_POD_ID_LEN
            )
        }
        Ok(())
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
                    wait_for_exit_code(CancellationToken::new(), pid).await;
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
        Self::unbind(self.base_path(), self.pod_id(), self.namespaces());

        if let Some(process) = &self.process {
            let pid = process.pid();
            info!("Killing pause PID: {pid}");
            match process.signal(Signal::SIGTERM) {
                Ok(()) => {}
                Err(Errno::ESRCH | Errno::ECHILD) => debug!("Pause PID {pid} already exited"),
                Err(e) => warn!("Unable to kill pause PID {pid}: {e:#}"),
            }
        }
    }

    /// Remove the bind mounts of the namespaces and the PID file of the init,
    /// which a stop or a failed setup leaves behind.
    fn unbind(base_path: &Path, pod_id: &str, namespaces: &[Namespace]) {
        for namespace in namespaces {
            if let Err(e) = namespace.umount(base_path, pod_id) {
                debug!("Unable to umount namespace {namespace}: {e:#}");
            }
        }

        let pause_pid_path = Self::pause_pid_path(base_path, pod_id);
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

        // On the blocking pool, a hung file system must not stall the
        // runtime thread and with it every request.
        let dir = base_path.to_path_buf();
        task::spawn_blocking(move || fs::create_dir_all(dir))
            .await
            .context("join creating base path")?
            .context("create base path")?;
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
            // The init of a PID namespace outlives this call, it does not
            // keep stderr open, see `run_child`. Only errors are of
            // interest, which end up in the error of a failed setup.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Do not leave it behind if the runtime drops this task on the
            // shutdown. Its child then stops the init of a PID namespace.
            .kill_on_drop(true)
            .spawn()
            .context("run pause")?;

        // The shutdown kills it if it takes too long. It is not reaped before
        // the wait below, so the PID is still its own here. Dropping it on a
        // failure kills it.
        let process = Process::open(child_pid(child.id()).context("PID of pause child")?);
        PAUSE.set_child(Some(process));
        let output = RuntimeOutput::spawn(None, child.stderr.take());
        let status = Self::wait_or_kill(&mut child, Self::SETUP_TIMEOUT).await;
        PAUSE.set_child(None);

        let mut pause = Self {
            base_path: base_path.to_path_buf(),
            pod_id: pod_id.into(),
            namespaces: namespaces.to_vec(),
            uid_mappings: uid_mappings.to_vec(),
            gid_mappings: gid_mappings.to_vec(),
            process: None,
            exited: Arc::default(),
        };

        let Some(status) = status else {
            // Stuck in the kernel, it gets reaped once it exits. The setup
            // may have bound namespaces already.
            task::spawn(async move { child.wait().await });
            stop_blocking(move || pause.stop()).await;
            bail!("pause did not exit after getting killed")
        };
        let status = status.context("wait for pause child")?;
        if !status.success() {
            // The pause subcommand undoes its own failed setup, but not if it
            // got killed by the shutdown. Its child stops the init of a PID
            // namespace then, the bind mounts and PID file stay.
            if status.signal().is_some() {
                stop_blocking(move || pause.stop()).await;
                bail!("exit status not ok: {status}")
            }
            let stderr = RuntimeOutput::finish(Some(output)).await;
            return Err(with_stderr(
                anyhow::format_err!("exit status not ok: {status}"),
                &stderr,
            ));
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

    /// How long the pause subcommand may take to set up the namespaces. A
    /// hung mount must not keep the requests which wait for it hanging.
    const SETUP_TIMEOUT: Duration = Duration::from_secs(60);

    /// Wait for the pause subcommand, kill it once the `timeout` passed and
    /// wait for that up to [`Pause::KILL_WAIT`]. None if it did not exit.
    async fn wait_or_kill(
        child: &mut tokio::process::Child,
        timeout: Duration,
    ) -> Option<std::io::Result<ExitStatus>> {
        if let Ok(status) = time::timeout(timeout, child.wait()).await {
            return Some(status);
        }
        warn!("Pause did not finish within {timeout:?}, killing it");
        if let Err(e) = child.start_kill() {
            debug!("Unable to kill pause child: {e}");
        }
        time::timeout(Self::KILL_WAIT, child.wait()).await.ok()
    }

    /// Read the PID of the init which the pause process left behind.
    fn read_pause_pid(base_path: &Path, pod_id: &str) -> Result<Pid> {
        read_pid_file(&Self::pause_pid_path(base_path, pod_id)).context("read pause PID")
    }

    /// Retrieve the pause PID path for a base and pod ID.
    fn pause_pid_path<T: AsRef<Path>>(base_path: T, pod_id: &str) -> PathBuf {
        // Not with set_extension, which would replace a part of a pod ID
        // with a dot.
        base_path
            .as_ref()
            .join("conmonrs")
            .join(format!("{pod_id}.pid"))
    }
}

impl Stop for Pause {
    fn stop(&self) {
        Pause::stop(self);
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
    pub fn as_str(self) -> &'static str {
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
    pub fn bind<T: AsRef<Path>>(self, path: T, pod_id: &str, pid: Pid) -> Result<()> {
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
    pub fn umount<T: AsRef<Path>>(self, path: T, pod_id: &str) -> Result<()> {
        let bind_path = self.path(path, pod_id);
        if let Err(e) = umount(&bind_path) {
            trace!("Unable to umount namespace {self}: {e:#}");
        }
        fs::remove_file(&bind_path).context("remove namespace bind path")
    }

    /// Retrieve the bind path of the namespace for the provided base path and pod ID.
    pub fn path<T: AsRef<Path>>(self, path: T, pod_id: &str) -> PathBuf {
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
mod tests;
