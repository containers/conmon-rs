//! Configuration related structures
use crate::{listener, process::is_zombie};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use nix::{
    errno::Errno,
    sys::{
        signal::kill,
        socket::{
            AddressFamily, SockFlag, SockType, UnixAddr, connect, getsockopt, socket,
            sockopt::PeerCredentials,
        },
    },
    unistd::Pid,
};
use std::{
    fmt, fs,
    io::ErrorKind,
    os::fd::{AsFd, AsRawFd},
    path::{self, Path, PathBuf},
};
use tracing_subscriber::filter::LevelFilter;

macro_rules! prefix {
    () => {
        "CONMON_"
    };
}

#[derive(Clone, Debug, Eq, Parser, PartialEq)]
#[command(
    after_help("More info at: https://github.com/containers/conmon-rs"),
    disable_version_flag(true)
)]
/// An OCI container runtime monitor.
pub struct Config {
    #[command(subcommand)]
    /// Possible subcommands.
    command: Option<Commands>,

    #[arg(
        default_missing_value("default"),
        env(concat!(prefix!(), "VERSION")),
        long("version"),
        num_args(0..=1),
        short('v'),
        value_enum,
        value_name("VERBOSITY")
    )]
    /// Show version information, specify "full" for verbose output.
    version: Option<Verbosity>,

    #[arg(
        default_missing_value("default"),
        env(concat!(prefix!(), "VERSION_JSON")),
        long("version-json"),
        visible_alias("vj"),
        short('j'),
        num_args(0..=1),
        value_enum,
        value_name("VERBOSITY")
    )]
    /// Show version information as parsable JSON, specify "full" for verbose output.
    version_json: Option<Verbosity>,

    #[arg(
        default_value_t,
        env(concat!(prefix!(), "LOG_LEVEL")),
        long("log-level"),
        short('l'),
        value_enum,
        value_name("LEVEL")
    )]
    /// The logging level of the conmon server.
    log_level: LogLevel,

    #[arg(
        default_value_t,
        env(concat!(prefix!(), "LOG_DRIVER")),
        long("log-driver"),
        short('d'),
        value_enum,
        value_name("DRIVER")
    )]
    /// The logging driver used by the conmon server.
    log_driver: LogDriver,

    // The blank default lets `--version` and `pause` work without the flag,
    // see `Config::validate`.
    #[arg(
        default_value(" "),
        env(concat!(prefix!(), "RUNTIME")),
        hide_default_value(true),
        long("runtime"),
        short('r'),
        value_name("RUNTIME")
    )]
    /// Binary path of the OCI runtime to use to operate on the containers.
    /// Required to run the server.
    runtime: PathBuf,

    #[arg(
        default_value(" "),
        env(concat!(prefix!(), "RUNTIME_DIR")),
        hide_default_value(true),
        long("runtime-dir"),
        value_name("RUNTIME_DIR")
    )]
    /// Path of the directory for conmonrs to hold files at runtime. Required
    /// to run the server.
    runtime_dir: PathBuf,

    #[arg(
        env(concat!(prefix!(), "RUNTIME_ROOT")),
        long("runtime-root"),
        value_name("RUNTIME_ROOT")
    )]
    /// Root directory used by the OCI runtime to operate on containers.
    runtime_root: Option<PathBuf>,

    #[arg(
        env(concat!(prefix!(), "SKIP_FORK")),
        long("skip-fork"),
        // The environment variable also accepts 1, 0, yes and no.
        value_parser = clap::builder::BoolishValueParser::new(),
    )]
    /// Do not fork if true.
    skip_fork: bool,

    #[arg(
        env(concat!(prefix!(), "ENABLE_TRACING")),
        long("enable-tracing"),
        short('e'),
    )]
    /// Enable OpenTelemetry tracing.
    enable_tracing: bool,

    #[arg(
        default_value("http://127.0.0.1:4317"),
        env(concat!(prefix!(), "TRACING_ENDPOINT")),
        long("tracing-endpoint"),
        short('t'),
        value_name("URL")
    )]
    /// OpenTelemetry GRPC endpoint to be used for tracing.
    tracing_endpoint: String,
}

impl Config {
    /// Possible subcommands.
    pub fn command(&self) -> Option<&Commands> {
        self.command.as_ref()
    }

    /// Show version information.
    pub fn version(&self) -> Option<Verbosity> {
        self.version
    }

    /// Show version information as JSON.
    pub fn version_json(&self) -> Option<Verbosity> {
        self.version_json
    }

    /// The logging level.
    pub fn log_level(&self) -> &LogLevel {
        &self.log_level
    }

    /// The logging driver.
    pub fn log_driver(&self) -> LogDriver {
        self.log_driver
    }

    /// Binary path of the OCI runtime.
    pub fn runtime(&self) -> &Path {
        &self.runtime
    }

    /// Path of the directory for conmonrs to hold files at runtime.
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Root directory used by the OCI runtime.
    pub fn runtime_root(&self) -> Option<&Path> {
        self.runtime_root.as_deref()
    }

    /// Do not fork if true.
    pub fn skip_fork(&self) -> bool {
        self.skip_fork
    }

    /// Enable OpenTelemetry tracing.
    #[cfg(feature = "tracing")]
    pub fn enable_tracing(&self) -> bool {
        self.enable_tracing
    }

    /// OpenTelemetry GRPC endpoint.
    #[cfg(feature = "tracing")]
    pub fn tracing_endpoint(&self) -> &str {
        &self.tracing_endpoint
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Subcommand)]
/// Possible subcommands.
pub enum Commands {
    /// Run pause, which bind mounts selected namespaces to the local file system.
    ///
    /// Only the namespaces selected by the flags are created and bind mounted to
    /// `$BASE_PATH/[ipc,pid,net,user,uts]ns/$POD_ID`, whereas the POD_ID is being passed from the
    /// client. The bind mounts keep the namespaces alive, so `conmonrs pause` exits once they
    /// are in place.
    ///
    /// Only a PID namespace (via `--pid`) dies with its init process, which therefore keeps
    /// running. Its PID is tracked in `$BASE_PATH/conmonrs/$POD_ID.pid`.
    ///
    /// UID and GID mappings are required if unsharing of the user namespace (via `--user`) is
    /// selected.
    Pause(PauseArgs),
}

#[derive(Args, Clone, Debug, Eq, PartialEq)]
/// The arguments of the `pause` subcommand.
pub struct PauseArgs {
    #[arg(
        default_value("/var/run"),
        env(concat!(prefix!(), "PAUSE_BASE_PATH")),
        long("base-path"),
        short('p'),
        value_name("PATH")
    )]
    /// The base path for pinning the namespaces.
    pub base_path: PathBuf,

    #[arg(
        env(concat!(prefix!(), "PAUSE_POD_ID")),
        long("pod-id"),
    )]
    /// The unique pod identifier for referring to the namespaces.
    pub pod_id: String,

    #[arg(long("ipc"))]
    /// Unshare the IPC namespace.
    pub ipc: bool,

    #[arg(long("pid"))]
    /// Unshare the PID namespace.
    pub pid: bool,

    #[arg(long("net"))]
    /// Unshare the network namespace.
    pub net: bool,

    #[arg(long("user"))]
    /// Unshare the user namespace.
    pub user: bool,

    #[arg(long("uts"))]
    /// Unshare the UTS namespace.
    pub uts: bool,

    #[arg(long("uid-mappings"), required_if_eq("user", "true"), short('u'))]
    /// User ID mappings for unsharing the user namespace.
    /// Allows multiple mappings in the format: "CONTAINER_ID HOST_ID SIZE".
    pub uid_mappings: Vec<String>,

    #[arg(long("gid-mappings"), required_if_eq("user", "true"), short('g'))]
    /// Group ID mappings for unsharing the user namespace.
    /// Allows multiple mappings in the format: "CONTAINER_ID HOST_ID SIZE".
    pub gid_mappings: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
/// Available log levels.
pub enum LogLevel {
    /// Trace level, the most verbose one.
    Trace,

    /// Debug level, less verbose than trace.
    Debug,

    #[default]
    /// Info level, less verbose than debug.
    Info,

    /// Warn level, less verbose than info.
    Warn,

    /// Error level, showing only errors.
    Error,

    /// Disable logging.
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
/// Available verbosity levels.
pub enum Verbosity {
    /// The default output verbosity.
    Default,

    /// The full output verbosity.
    Full,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
/// Available log drivers.
pub enum LogDriver {
    /// Use no log driver.
    None,

    /// Use stdout as log driver.
    Stdout,

    #[default]
    /// Use systemd journald as log driver
    Systemd,
}

/// Formats the level with the name clap derives from the variant, which is
/// what the command line and the runtime flags use.
impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self
            .to_possible_value()
            .expect("LogLevel has no skipped variants");
        f.write_str(value.get_name())
    }
}

impl From<LogLevel> for LevelFilter {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Trace => Self::TRACE,
            LogLevel::Debug => Self::DEBUG,
            LogLevel::Info => Self::INFO,
            LogLevel::Warn => Self::WARN,
            LogLevel::Error => Self::ERROR,
            LogLevel::Off => Self::OFF,
        }
    }
}

impl Config {
    /// Parse the configuration from the command line and the environment.
    pub fn parse_args() -> Self {
        Self::parse()
    }
}

// Sync with `pkg/client/client.go`
const SOCKET: &str = "conmon.sock";
const PIDFILE: &str = "pidfile";
const FD_SOCKET: &str = "conmon-fd.sock";

impl Config {
    /// Validate the configuration integrity.
    pub fn validate(&self) -> Result<()> {
        // The runtime flags default to a blank, so that `--version` and the
        // `pause` subcommand work without them.
        const RUNTIME_FLAG: &str = "--runtime";
        if self
            .runtime()
            .to_str()
            .with_context(|| format!("{RUNTIME_FLAG} does not parse as string"))?
            .trim()
            .is_empty()
        {
            bail!("{} flag not set", RUNTIME_FLAG)
        }

        const RUNTIME_DIR_FLAG: &str = "--runtime-dir";
        if self
            .runtime_dir()
            .to_str()
            .with_context(|| format!("{RUNTIME_DIR_FLAG} does not parse as string"))?
            .trim()
            .is_empty()
        {
            bail!("{} flag not set", RUNTIME_DIR_FLAG)
        }

        if !self.runtime().exists() {
            bail!(
                "{} '{}' does not exist",
                RUNTIME_FLAG,
                self.runtime().display()
            )
        }

        const RUNTIME_ROOT_FLAG: &str = "--runtime-root";
        if let Some(rr) = self.runtime_root()
            && rr.exists()
            && !rr.is_dir()
        {
            bail!(
                "{} '{}' is not a directory",
                RUNTIME_ROOT_FLAG,
                rr.display()
            )
        }

        Ok(())
    }

    /// Make the paths absolute, the server changes into the root directory
    /// once it started, see `Server::start`.
    pub fn make_absolute(&mut self) -> Result<()> {
        self.runtime = path::absolute(&self.runtime).context("make runtime path absolute")?;
        self.runtime_dir =
            path::absolute(&self.runtime_dir).context("make runtime dir absolute")?;
        if let Some(rr) = &mut self.runtime_root {
            *rr = path::absolute(&*rr).context("make runtime root absolute")?;
        }
        Ok(())
    }

    /// Create the runtime directories and remove stale sockets. Separate from
    /// [`Config::validate`], which only checks and changes nothing on disk.
    pub fn prepare(&self) -> Result<()> {
        fs::create_dir_all(self.runtime_dir()).context("create runtime dir")?;

        if let Some(rr) = self.runtime_root() {
            fs::create_dir_all(rr).context("create runtime root")?;
        }

        // A socket left behind gets replaced, but not the one of a server
        // which still runs, its containers would be lost otherwise.
        for (socket, typ) in [
            (self.socket(), SockType::Stream),
            (self.fd_socket(), SockType::SeqPacket),
        ] {
            // A socket which cannot be checked may be in use as well.
            if Self::socket_in_use(&socket, typ)
                .with_context(|| format!("check socket {}", socket.display()))?
            {
                bail!("socket {} is in use by a running server", socket.display())
            }
            match fs::remove_file(&socket) {
                Err(e) if e.kind() != ErrorKind::NotFound => {
                    return Err(e)
                        .with_context(|| format!("remove stale socket {}", socket.display()));
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Whether a server accepts connections on the socket. The connect does
    /// not wait for a server which does not accept them in time, a full
    /// backlog counts as in use as well. A socket of a server which exited
    /// is not in use, also if a process left behind still holds it.
    fn socket_in_use(path: &Path, typ: SockType) -> Result<bool> {
        // The path may be too long for a socket address.
        let (short_path, _dir) = listener::shorten_path(path)?;
        let fd = socket(
            AddressFamily::Unix,
            typ,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            None,
        )
        .context("create socket")?;
        let addr = UnixAddr::new(&short_path).context("create socket address")?;
        Ok(match connect(fd.as_raw_fd(), &addr) {
            Ok(()) => Self::peer_running(&fd),
            Err(Errno::EAGAIN) => true,
            // Nothing which this server could reach serves it: the socket
            // is gone, left behind or something else.
            Err(Errno::ECONNREFUSED | Errno::ENOENT | Errno::ENOTSOCK | Errno::EPROTOTYPE) => false,
            // Like one of another user left behind, removing it only needs
            // the permissions of the directory. The logging is not set up yet,
            // so it goes to stderr.
            Err(Errno::EACCES) => {
                eprintln!(
                    "Replacing socket {} which is not accessible",
                    path.display()
                );
                false
            }
            // Running out of resources tells nothing about the socket.
            Err(e) => return Err(e).context("connect socket"),
        })
    }

    /// Whether the process which listens on the other end of a connection
    /// still runs, true if that cannot be told.
    fn peer_running(fd: &impl AsFd) -> bool {
        let Ok(credentials) = getsockopt(fd, PeerCredentials) else {
            return true;
        };
        let pid = credentials.pid();
        // From another PID namespace.
        if pid <= 0 {
            return true;
        }
        // A zombie does not accept either.
        let pid = Pid::from_raw(pid);
        kill(pid, None) != Err(Errno::ESRCH) && !is_zombie(pid)
    }
    pub fn socket(&self) -> PathBuf {
        self.runtime_dir().join(SOCKET)
    }
    pub fn conmon_pidfile(&self) -> PathBuf {
        self.runtime_dir().join(PIDFILE)
    }
    pub fn fd_socket(&self) -> PathBuf {
        self.runtime_dir().join(FD_SOCKET)
    }
}

impl Config {
    /// Parse a configuration from the arguments for a test. None if `CONMON_*`
    /// variables are set, the flags also come from the environment, which
    /// would change what the parsed configuration contains.
    #[cfg(test)]
    pub(crate) fn parse_for_test<I, T>(args: I) -> Option<Self>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        if std::env::vars_os().any(|(key, _)| key.to_string_lossy().starts_with(prefix!())) {
            eprintln!("skipping, CONMON_* variables are set");
            return None;
        }
        let args = std::iter::once("conmonrs".into()).chain(args.into_iter().map(Into::into));
        Some(Self::try_parse_from(args).expect("parse args"))
    }
}

#[cfg(test)]
mod tests;
