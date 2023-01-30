use anyhow::{bail, Context, Result};
use capnp::enum_list::Reader;
use conmon_common::conmon_capnp::conmon;
use getset::{CopyGetters, Getters};
use libc::pid_t;
use nix::{
    mount::{mount, umount, MsFlags},
    sched::{unshare, CloneFlags},
    sys::signal::{kill, Signal},
    unistd::{fork, setresgid, setresuid, ForkResult, Gid, Pid, Uid},
};
use once_cell::sync::OnceCell;
use signal_hook::{consts::TERM_SIGNALS, iterator::Signals};
use std::{
    env,
    fs::{self, File},
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{exit, Command},
};
use strum::{AsRefStr, Display, EnumIter, EnumString, IntoStaticStr};
use tracing::{debug, error, info};
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
    pub fn init_shared(
        namespaces: Reader<conmon::Namespace>,
        uid_mappings: Vec<String>,
        gid_mappings: Vec<String>,
    ) -> Result<&'static Pause> {
        PAUSE.get_or_try_init(|| {
            Self::init(namespaces, uid_mappings, gid_mappings).context("init pause")
        })
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
    fn init(
        init_namespaces: Reader<conmon::Namespace>,
        uid_mappings: Vec<String>,
        gid_mappings: Vec<String>,
    ) -> Result<Self> {
        debug!("Initializing pause");

        let mut args: Vec<String> = vec![];
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

    #[allow(clippy::too_many_arguments)]
    /// Run a new pause instance.
    pub fn run<T: AsRef<Path> + Copy>(
        path: T,
        ipc: bool,
        pid: bool,
        net: bool,
        user: bool,
        uts: bool,
        uid_mappings: &[String],
        gid_mappings: &[String],
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
            // CLONE_NEWNS is intentional here, because we need a new mount namespace for user
            // namespace handling as well. The CLONE_NEWUSER will be done before calling unshare
            // with the rest of the flags.
            flags.insert(CloneFlags::CLONE_NEWNS);
            namespaces.push(Namespace::User);
        }
        if uts {
            flags.insert(CloneFlags::CLONE_NEWUTS);
            namespaces.push(Namespace::Uts);
        }

        if !user {
            unshare(flags).context("unshare with clone flags")?;
        }

        let (mut sock_parent, mut sock_child) =
            UnixStream::pair().context("create unix socket pair")?;
        const MSG: &[u8] = &[1];

        match unsafe { fork().context("forking process")? } {
            ForkResult::Parent { child } => {
                let mut file = File::create(path.as_ref().join(PAUSE_PID_FILE))
                    .context("create pause PID file")?;
                write!(file, "{child}").context("write child to pause file")?;

                if user {
                    // Wait for user namespace creation
                    let mut res = [0];
                    sock_parent.read_exact(&mut res)?;

                    // Write mappings
                    Self::write_mappings(gid_mappings, child, true).context("write gid maps")?;
                    Self::write_mappings(uid_mappings, child, false).context("write uid maps")?;

                    // Notify that user mappings have been written
                    sock_parent.write_all(MSG)?;
                }

                exit(0);
            }

            ForkResult::Child if user => {
                unshare(CloneFlags::CLONE_NEWUSER).context("unshare into user namespace")?;

                // Notify that the user namespace is now created
                sock_child.write_all(MSG)?;

                // Wait for the mappings to be written
                let mut res = [0];
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

    /// Write user or group ID mappings.
    fn write_mappings(mappings: &[String], pid: Pid, is_group: bool) -> Result<()> {
        let path = PathBuf::from("/proc")
            .join(pid.to_string())
            .join(if is_group { "gid_map" } else { "uid_map" });

        let mut file = File::options()
            .write(true)
            .open(&path)
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
