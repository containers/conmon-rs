use anyhow::{Context, Result, bail};
use capnp::enum_list::Reader;
use conmon_common::conmon_capnp::conmon;
use libc::pid_t;
use nix::{
    mount::{MsFlags, mount, umount},
    sched::{CloneFlags, unshare},
    sys::signal::{Signal, kill},
    unistd::{ForkResult, Gid, Pid, Uid, fork, setresgid, setresuid},
};
use signal_hook::{consts::TERM_SIGNALS, iterator::Signals};
use std::{
    env,
    fs::{self, File},
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, exit},
    sync::OnceLock,
};
use strum::{AsRefStr, Display, EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};
use tracing::{debug, info, trace, warn};

/// The main structure for this module.
#[derive(Debug)]
pub struct Pause {
    base_path: PathBuf,
    pod_id: Box<str>,
    namespaces: Vec<Namespace>,
    pid: Pid,
}

impl Pause {
    pub fn base_path(&self) -> &PathBuf {
        &self.base_path
    }
    pub fn pod_id(&self) -> &str {
        &self.pod_id
    }
    pub fn namespaces(&self) -> &[Namespace] {
        &self.namespaces
    }
    pub fn pid(&self) -> Pid {
        self.pid
    }
}

/// The global shared multiple pause instance.
static PAUSE: OnceLock<Pause> = OnceLock::new();

impl Pause {
    /// Retrieve the global instance of pause
    pub fn init_shared(
        base_path: &str,
        pod_id: &str,
        namespaces: Reader<conmon::Namespace>,
        uid_mappings: Vec<String>,
        gid_mappings: Vec<String>,
    ) -> Result<&'static Pause> {
        if let Some(pause) = PAUSE.get() {
            return Ok(pause);
        }
        let pause = Self::init(base_path, pod_id, namespaces, uid_mappings, gid_mappings)
            .context("init pause")?;
        let _ = PAUSE.set(pause);
        PAUSE.get().context("pause not initialized")
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

        info!("Killing pause PID: {}", self.pid());
        if let Err(e) = kill(self.pid(), Signal::SIGTERM) {
            warn!("Unable to kill pause PID {}: {:#}", self.pid(), e);
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
        base_path: &str,
        pod_id: &str,
        init_namespaces: Reader<conmon::Namespace>,
        uid_mappings: Vec<String>,
        gid_mappings: Vec<String>,
    ) -> Result<Self> {
        debug!("Initializing pause");

        let mut args: Vec<String> = vec![format!("--pod-id={pod_id}")];
        let mut namespaces = vec![];

        for namespace in init_namespaces.iter() {
            match namespace? {
                conmon::Namespace::Ipc => {
                    args.push("--ipc".into());
                    namespaces.push(Namespace::Ipc);
                }
                conmon::Namespace::Net => {
                    args.push("--net".into());
                    namespaces.push(Namespace::Net);
                }
                conmon::Namespace::Pid => {
                    args.push("--pid".into());
                    namespaces.push(Namespace::Pid);
                }
                conmon::Namespace::Uts => {
                    args.push("--uts".into());
                    namespaces.push(Namespace::Uts);
                }
                conmon::Namespace::User => {
                    if uid_mappings.is_empty() {
                        bail!("user ID mappings are empty")
                    }

                    if gid_mappings.is_empty() {
                        bail!("group ID mappings are empty")
                    }

                    args.push("--user".into());

                    for mapping in &uid_mappings {
                        args.push(format!("--uid-mappings={mapping}"));
                    }

                    for mapping in &gid_mappings {
                        args.push(format!("--gid-mappings={mapping}"));
                    }

                    namespaces.push(Namespace::User);
                }
            }
        }
        debug!("Pause namespaces: {:?}", namespaces);
        debug!("Pause args: {:?}", args);

        let base_path = PathBuf::from(base_path);
        fs::create_dir_all(&base_path).context("create base path")?;
        debug!("Pause base path: {}", base_path.display());

        let program = env::args().next().context("no args set")?;
        let mut child = Command::new(program)
            .arg("pause")
            .arg("--base-path")
            .arg(&base_path)
            .args(args)
            .spawn()
            .context("run pause")?;

        let status = child.wait().context("wait for pause child")?;
        if !status.success() {
            bail!("exit status not ok: {status}")
        }

        let pid = fs::read_to_string(Self::pause_pid_path(&base_path, pod_id))
            .context("read pause PID path")?
            .trim()
            .parse::<u32>()
            .context("parse pause PID")?;
        info!("Pause PID is: {pid}");

        Ok(Self {
            base_path,
            pod_id: pod_id.to_owned().into_boxed_str(),
            namespaces: Namespace::iter().collect(),
            pid: Pid::from_raw(pid as pid_t),
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
        if user {
            // CLONE_NEWNS is intentional here, because we need a new mount namespace for user
            // namespace handling as well. The CLONE_NEWUSER will be done before calling unshare
            // with the rest of the flags.
            flags.insert(CloneFlags::CLONE_NEWNS);
        }
        if uts {
            flags.insert(CloneFlags::CLONE_NEWUTS);
        }

        if !user {
            unshare(flags).context("unshare with clone flags")?;
        }

        let (mut sock_parent, mut sock_child) =
            UnixStream::pair().context("create unix socket pair")?;
        const MSG: &[u8] = &[1];
        let mut res = [0];

        match unsafe { fork().context("forking process")? } {
            ForkResult::Parent { child } => {
                let pause_pid_path = Self::pause_pid_path(base_path, pod_id);
                fs::create_dir_all(
                    pause_pid_path
                        .parent()
                        .context("no parent for pause PID path")?,
                )
                .context("create pause PID parent path")?;
                let mut file = File::create(pause_pid_path).context("create pause PID file")?;
                write!(file, "{child}").context("write child to pause file")?;

                if user {
                    // Wait for user namespace creation
                    sock_parent.read_exact(&mut res)?;

                    // Write mappings
                    Self::write_mappings(gid_mappings, child, true).context("write gid maps")?;
                    Self::write_mappings(uid_mappings, child, false).context("write uid maps")?;

                    // Notify that user mappings have been written
                    sock_parent.write_all(MSG)?;
                }

                // Wait for mounts to be created
                sock_parent.read_exact(&mut res)?;

                exit(0);
            }

            ForkResult::Child if user => {
                unshare(CloneFlags::CLONE_NEWUSER).context("unshare into user namespace")?;

                // Notify that the user namespace is now created
                sock_child.write_all(MSG)?;

                // Wait for the mappings to be written
                sock_child.read_exact(&mut res)?;

                // Set the UID and GID
                let uid = Uid::from_raw(0);
                setresuid(uid, uid, uid).context("set root uid")?;

                let gid = Gid::from_raw(0);
                setresgid(gid, gid, gid).context("set root gid")?;

                // Unshare the rest of the namespaces
                unshare(flags).context("unshare with other clone flags")?;
            }

            _ => (),
        }

        // We bind all namespaces, if not unshared then we use the host namespace.
        for namespace in Namespace::iter() {
            namespace.bind(base_path, pod_id).with_context(|| {
                format!(
                    "bind namespace to path: {}",
                    namespace.path(base_path, pod_id).display(),
                )
            })?;
        }

        // Notify that all mounts are created
        sock_child.write_all(MSG)?;

        let mut signals = Signals::new(TERM_SIGNALS).context("register signals")?;
        signals.forever().next().context("no signal number")?;
        Ok(())
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

#[derive(
    AsRefStr, Clone, Copy, Debug, Display, EnumIter, EnumString, Eq, IntoStaticStr, PartialEq,
)]
#[strum(serialize_all = "lowercase")]
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

impl Namespace {
    /// Bind the namespace to the provided base path and pod ID.
    pub fn bind<T: AsRef<Path>>(&self, path: T, pod_id: &str) -> Result<()> {
        let bind_path = self.path(path, pod_id);
        fs::create_dir_all(
            bind_path
                .parent()
                .context("no parent namespace bind path")?,
        )
        .context("create namespace parent path")?;
        File::create(&bind_path).context("create namespace bind path")?;
        let source_path = PathBuf::from("/proc/self/ns").join(self.as_ref());

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
