//! Configuration related structures
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use getset::{CopyGetters, Getters, Setters};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
use strum::{AsRefStr, Display, EnumIter, EnumString, IntoStaticStr};

macro_rules! prefix {
    () => {
        "CONMON_"
    };
}

#[derive(CopyGetters, Debug, Deserialize, Eq, Getters, Parser, PartialEq, Serialize, Setters)]
#[serde(rename_all = "kebab-case")]
#[command(
    after_help("More info at: https://github.com/containers/conmon-rs"),
    disable_version_flag(true)
)]
/// An OCI container runtime monitor.
pub struct Config {
    #[get = "pub"]
    #[command(subcommand)]
    /// Possible subcommands.
    command: Option<Commands>,

    #[get_copy = "pub"]
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

    #[get = "pub"]
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

    #[get_copy = "pub"]
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

    #[get = "pub"]
    #[arg(
        default_value(" "),
        env(concat!(prefix!(), "RUNTIME")),
        long("runtime"),
        short('r'),
        value_name("RUNTIME")
    )]
    /// Binary path of the OCI runtime to use to operate on the containers.
    runtime: PathBuf,

    #[get = "pub"]
    #[arg(
        default_value(" "),
        env(concat!(prefix!(), "RUNTIME_DIR")),
        long("runtime-dir"),
        value_name("RUNTIME_DIR")
    )]
    /// Path of the directory for conmonrs to hold files at runtime.
    runtime_dir: PathBuf,

    #[get = "pub"]
    #[arg(
        env(concat!(prefix!(), "RUNTIME_ROOT")),
        long("runtime-root"),
        value_name("RUNTIME_ROOT")
    )]
    /// Root directory used by the OCI runtime to operate on containers.
    runtime_root: Option<PathBuf>,

    #[get_copy = "pub"]
    #[arg(
        env(concat!(prefix!(), "SKIP_FORK")),
        long("skip-fork"),
        value_name("SKIP_FORK")
    )]
    /// Do not fork if true.
    skip_fork: bool,

    // TODO: remove in next major release
    #[arg(default_value(""), long("cgroup-manager"), short('c'), hide(true))]
    /// (ignored for backwards compatibility)
    cgroup_manager: String,

    #[get_copy = "pub"]
    #[arg(
        env(concat!(prefix!(), "ENABLE_TRACING")),
        long("enable-tracing"),
        short('e'),
    )]
    /// Enable OpenTelemetry tracing.
    enable_tracing: bool,

    #[get = "pub"]
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, Subcommand)]
/// Possible subcommands.
pub enum Commands {
    /// Run pause, which bind mounts selected namespaces to the local file system.
    ///
    /// If a namespace is not selected by one of the flags, then it will fallback to the host
    /// namespace and still create the bind mount to it. All namespaces are mounted to
    /// /var/run/[ipc,pid,net,user,uts]ns/$POD_ID, whereas the POD_ID is being passed from the
    /// client.
    ///
    /// Tracking of the pause PID will be done by using a file in /var/run/conmonrs/$POD_ID.pid,
    /// which gets removed together with the mounted namespaces if `conmonrs pause` terminates.
    ///
    /// UID and GID mappings are required if unsharing of the user namespace (via `--user`) is
    /// selected.
    Pause {
        #[arg(
            default_value("/var/run"),
            env(concat!(prefix!(), "PAUSE_BASE_PATH")),
            long("base-path"),
            short('p'),
            value_name("PATH")
        )]
        /// The base path for pinning the namespaces.
        base_path: PathBuf,

        #[arg(
            env(concat!(prefix!(), "PAUSE_POD_ID")),
            long("pod-id"),
        )]
        /// The unique pod identifier for referring to the namespaces.
        pod_id: String,

        #[arg(long("ipc"))]
        /// Unshare the IPC namespace.
        ipc: bool,

        #[arg(long("pid"))]
        /// Unshare the PID namespace.
        pid: bool,

        #[arg(long("net"))]
        /// Unshare the network namespace.
        net: bool,

        #[arg(long("user"))]
        /// Unshare the user namespace.
        user: bool,

        #[arg(long("uts"))]
        /// Unshare the UTS namespace.
        uts: bool,

        #[arg(long("uid-mappings"), required_if_eq("user", "true"), short('u'))]
        /// User ID mappings for unsahring the user namespace.
        /// Allows multiple mappings in the format: "CONTAINER_ID HOST_ID SIZE".
        uid_mappings: Vec<String>,

        #[arg(long("gid-mappings"), required_if_eq("user", "true"), short('g'))]
        /// Group ID mappings for unsahring the user namespace.
        /// Allows multiple mappings in the format: "CONTAINER_ID HOST_ID SIZE".
        gid_mappings: Vec<String>,
    },
}

#[derive(
    AsRefStr,
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    EnumIter,
    EnumString,
    Eq,
    Hash,
    IntoStaticStr,
    PartialEq,
    Serialize,
    ValueEnum,
)]
#[strum(serialize_all = "lowercase")]
/// Available log levels.
pub enum LogLevel {
    /// Trace level, the most verbose one.
    Trace,

    /// Debug level, less verbose than trace.
    Debug,

    /// Info level, less verbose than debug.
    Info,

    /// Warn level, less verbose than info.
    Warn,

    /// Error level, showing only errors.
    Error,

    /// Disable logging.
    Off,
}

impl Default for LogLevel {
    fn default() -> Self {
        Self::Info
    }
}

#[derive(
    AsRefStr,
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    EnumIter,
    EnumString,
    Eq,
    Hash,
    IntoStaticStr,
    PartialEq,
    Serialize,
    ValueEnum,
)]
#[strum(serialize_all = "lowercase")]
/// Available verbosity levels.
pub enum Verbosity {
    /// The default output verbosity.
    Default,

    /// The full output verbosity.
    Full,
}

#[derive(
    AsRefStr,
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    EnumIter,
    EnumString,
    Eq,
    Hash,
    IntoStaticStr,
    PartialEq,
    Serialize,
    ValueEnum,
)]
#[strum(serialize_all = "lowercase")]
/// Available log drivers.
pub enum LogDriver {
    /// Use stdout as log driver.
    Stdout,

    /// Use systemd journald as log driver
    Systemd,
}

impl Default for LogDriver {
    fn default() -> Self {
        Self::Systemd
    }
}

impl Default for Config {
    fn default() -> Self {
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
        const RUNTIME_FLAG: &str = "--runtime";
        if self
            .runtime()
            .to_str()
            .context(format!("{} does not parse as string", RUNTIME_FLAG))?
            .trim()
            .is_empty()
        {
            bail!("{} flag not set", RUNTIME_FLAG)
        }

        const RUNTIME_DIR_FLAG: &str = "--runtime-dir";
        if self
            .runtime_dir()
            .to_str()
            .context(format!("{} does not parse as string", RUNTIME_DIR_FLAG))?
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
        if !self.runtime_dir().exists() {
            fs::create_dir_all(self.runtime_dir())?;
        }

        if let Some(rr) = self.runtime_root() {
            if !rr.exists() {
                fs::create_dir_all(rr)?;
            } else if !rr.is_dir() {
                bail!("{} '{}' does not exist", RUNTIME_ROOT_FLAG, rr.display())
            }
        }

        if self.socket().exists() {
            fs::remove_file(self.socket())?;
        }

        if self.fd_socket().exists() {
            fs::remove_file(self.fd_socket())?;
        }

        Ok(())
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
