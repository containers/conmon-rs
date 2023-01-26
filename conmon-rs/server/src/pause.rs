use anyhow::{bail, Context, Result};
use capnp::enum_list::Reader;
use conmon_common::conmon_capnp::conmon;
use getset::{CopyGetters, Getters};
use libc::pid_t;
use nix::{
    mount::{mount, umount, MsFlags},
    sched::{unshare, CloneFlags},
    sys::signal::{kill, Signal},
    unistd::{fork, ForkResult, Pid},
};
use once_cell::sync::OnceCell;
use signal_hook::{consts::TERM_SIGNALS, iterator::Signals};
use std::{
    env,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{exit, Command},
};
use strum::{AsRefStr, Display, EnumIter, EnumString, IntoStaticStr};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// The main structure for this module.
#[derive(Debug, CopyGetters, Getters)]
pub struct Pause {
    #[get = "pub"]
    path: PathBuf,

    #[get = "pub"]
    namespaces: Vec<Namespace>,

    #[get_copy]
    pid: Pid,
}

/// The global shared multiple pause instance.
static PAUSE: OnceCell<Pause> = OnceCell::new();

/// The global path for storing bin mounted namespaces.
const PAUSE_PATH: &str = "/var/run/conmonrs";

/// The file path for storing the pause PID.
const PAUSE_PID_FILE: &str = ".pause_pid";

impl Pause {
    /// Retrieve the global instance of pause
    pub fn init_shared(namespaces: Reader<conmon::Namespace>) -> Result<&'static Pause> {
        PAUSE.get_or_try_init(|| Self::init(namespaces).context("init pause"))
    }

    /// Retrieve the global instance of pause if initialized.
    pub fn maybe_shared() -> Option<&'static Pause> {
        PAUSE.get()
    }

    /// Stop the global pause instance.
    pub fn stop(&self) {
        info!("Stopping pause");
        for namespace in self.namespaces() {
            if let Err(e) = namespace.umount(self.path()) {
                debug!("Unable to umount namespace {namespace}: {:#}", e);
            }
        }
        if let Err(e) = fs::remove_dir_all(self.path()) {
            error!(
                "Unable to remove pause path {}: {:#}",
                self.path().display(),
                e
            );
        }

        info!("Killing pause PID: {}", self.pid());
        if let Err(e) = kill(self.pid(), Signal::SIGTERM) {
            error!("Unable to kill pause PID {}: {:#}", self.pid(), e);
        }
    }

    /// Initialize a new pause instance.
    fn init(init_namespaces: Reader<conmon::Namespace>) -> Result<Self> {
        debug!("Initializing pause");

        let mut args = vec![];
        let mut namespaces = vec![];
        for namespace in init_namespaces.iter() {
            match namespace? {
                conmon::Namespace::Ipc => {
                    args.push("--ipc");
                    namespaces.push(Namespace::Ipc);
                }
                conmon::Namespace::Net => {
                    args.push("--net");
                    namespaces.push(Namespace::Net);
                }
                conmon::Namespace::Pid => {
                    args.push("--pid");
                    namespaces.push(Namespace::Pid);
                }
                conmon::Namespace::Uts => {
                    args.push("--uts");
                    namespaces.push(Namespace::Uts);
                }
                conmon::Namespace::User => {
                    warn!("Unsharing the user namespace is not supported yet");
                    // args.push("--user");
                    // namespaces.push(Namespace::User);
                }
            }
        }
        debug!("Pause namespaces: {:?}", namespaces);

        let path = PathBuf::from(PAUSE_PATH).join(Uuid::new_v4().to_string());
        fs::create_dir_all(&path).context("create base path")?;
        debug!("Pause base path: {}", path.display());

        let program = env::args().next().context("no args set")?;
        let mut child = Command::new(program)
            .arg("pause")
            .arg("--path")
            .arg(&path)
            .args(args)
            .spawn()
            .context("run pause")?;

        let status = child.wait().context("wait for pause child")?;
        if !status.success() {
            bail!("exit status not ok: {status}")
        }

        let pid = fs::read_to_string(path.join(PAUSE_PID_FILE))
            .context("read pause PID path")?
            .trim()
            .parse::<u32>()
            .context("parse pause PID")?;
        info!("Pause PID is: {pid}");

        Ok(Self {
            path,
            namespaces,
            pid: Pid::from_raw(pid as pid_t),
        })
    }

    /// Run a new pause instance.
    pub fn run<T: AsRef<Path> + Copy>(
        path: T,
        ipc: bool,
        pid: bool,
        net: bool,
        user: bool,
        uts: bool,
    ) -> Result<()> {
        let mut namespaces = vec![];
        let mut flags = CloneFlags::empty();
        if ipc {
            flags.insert(CloneFlags::CLONE_NEWIPC);
            namespaces.push(Namespace::Ipc);
        }
        if pid {
            flags.insert(CloneFlags::CLONE_NEWPID);
            namespaces.push(Namespace::Pid);
        }
        if net {
            flags.insert(CloneFlags::CLONE_NEWNET);
            namespaces.push(Namespace::Net);
        }
        if user {
            // Unsharing the user namespace is not supported yet
            // flags.insert(CloneFlags::CLONE_NEWUSER);
            // namespaces.push(Namespace::User);
        }
        if uts {
            flags.insert(CloneFlags::CLONE_NEWUTS);
            namespaces.push(Namespace::Uts);
        }

        unshare(flags).context("unshare with clone flags")?;

        match unsafe { fork().context("forking process")? } {
            ForkResult::Parent { child } => {
                let mut file = File::create(path.as_ref().join(PAUSE_PID_FILE))
                    .context("create pause PID file")?;
                write!(file, "{child}").context("write child to pause file")?;
                exit(0);
            }
            ForkResult::Child => (),
        }

        for namespace in namespaces {
            namespace.bind(path.as_ref()).context(format!(
                "bind namespace to path: {}",
                namespace.path(path).display(),
            ))?;
        }

        let mut signals = Signals::new(TERM_SIGNALS).context("register signals")?;
        signals.forever().next().context("no signal number")?;
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

    /// The network namespace. The namespace is empty and has no conectivity, even localhost
    /// network, unless some setup is done afterwards.
    Net,

    /// The user namespace, which allows to segregate the user ID.
    User,

    /// The UTS namespace, which allows to change hostname of the new container.
    Uts,
}

impl Namespace {
    /// Bind the namespace to the provided base path.
    pub fn bind<T: AsRef<Path>>(&self, path: T) -> Result<()> {
        let bind_path = self.path(path);
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
    pub fn umount<T: AsRef<Path>>(&self, path: T) -> Result<()> {
        let bind_path = self.path(path);
        umount(&bind_path).context("umount namespace")
    }

    /// Retrieve the bind path of the namespace for the provided base path.
    pub fn path<T: AsRef<Path>>(&self, path: T) -> PathBuf {
        path.as_ref().join(self.as_ref())
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
