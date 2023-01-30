//! Configuration related structures
use anyhow::{bail, Result};
use clap::{ArgEnum, Parser, Subcommand};
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
#[clap(
    after_help("More info at: https://github.com/containers/conmon-rs"),
    disable_version_flag(true)
)]
/// An OCI container runtime monitor.
pub struct Config {
    #[get = "pub"]
    #[clap(subcommand)]
    /// Possible subcommands.
    command: Option<Commands>,

    #[get_copy = "pub"]
    #[clap(
        default_missing_value("default"),
        env(concat!(prefix!(), "VERSION")),
        long("version"),
        short('v'),
        value_enum,
        value_name("VERBOSITY")
    )]
    /// Show version information, specify "full" for verbose output.
    version: Option<Verbosity>,

    #[get = "pub"]
    #[clap(
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
    #[clap(
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
    #[clap(
        default_value(""),
        env(concat!(prefix!(), "RUNTIME")),
        long("runtime"),
        short('r'),
        value_name("RUNTIME")
    )]
    /// Binary path of the OCI runtime to use to operate on the containers.
    runtime: PathBuf,

    #[get = "pub"]
    #[clap(
        default_value(""),
        env(concat!(prefix!(), "RUNTIME_DIR")),
        long("runtime-dir"),
        value_name("RUNTIME_DIR")
    )]
    /// Path of the directory for conmonrs to hold files at runtime.
    runtime_dir: PathBuf,

    #[get = "pub"]
    #[clap(
        env(concat!(prefix!(), "RUNTIME_ROOT")),
        long("runtime-root"),
        value_name("RUNTIME_ROOT")
    )]
    /// Root directory used by the OCI runtime to operate on containers.
    runtime_root: Option<PathBuf>,

    #[get_copy = "pub"]
    #[clap(
        env(concat!(prefix!(), "SKIP_FORK")),
        long("skip-fork"),
        value_name("SKIP_FORK")
    )]
    /// Do not fork if true.
    skip_fork: bool,

    #[get_copy = "pub"]
    #[clap(
        default_value_t,
        env(concat!(prefix!(), "CGROUP_MANAGER")),
        long("cgroup-manager"),
        short('c'),
        value_enum,
        value_name("MANAGER")
    )]
    /// Select the cgroup manager to be used.
    cgroup_manager: CgroupManager,

    #[get_copy = "pub"]
    #[clap(
        env(concat!(prefix!(), "ENABLE_TRACING")),
        long("enable-tracing"),
        short('e'),
    )]
    /// Enable OpenTelemetry tracing.
    enable_tracing: bool,

    #[get = "pub"]
    #[clap(
        default_value("http://localhost:4317"),
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
    /// Run pause instead of the server.
    Pause {
        #[clap(
            env(concat!(prefix!(), "PAUSE_PATH")),
            long("path"),
            short('p'),
            value_name("PATH")
        )]
        /// The base path for pinning the namespaces.
        path: PathBuf,

        #[clap(long("ipc"))]
        /// Unshare the IPC namespace.
        ipc: bool,

        #[clap(long("pid"))]
        /// Unshare the PID namespace.
        pid: bool,

        #[clap(long("net"))]
        /// Unshare the network namespace.
        net: bool,

        #[clap(long("user"))]
        /// Unshare the user namespace.
        user: bool,

        #[clap(long("uts"))]
        /// Unshare the UTS namespace.
        uts: bool,

        #[clap(long("uid-mappings"), required_if_eq("user", "true"), short('u'))]
        /// User ID mappings for unsahring the user namespace.
        /// Allows multiple mappings in the format: "CONTAINER_ID HOST_ID SIZE".
        uid_mappings: Vec<String>,

        #[clap(long("gid-mappings"), required_if_eq("user", "true"), short('g'))]
        /// Group ID mappings for unsahring the user namespace.
        /// Allows multiple mappings in the format: "CONTAINER_ID HOST_ID SIZE".
        gid_mappings: Vec<String>,
    },
}

#[derive(
    ArgEnum,
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
    ArgEnum,
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
    ArgEnum,
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

#[derive(
    ArgEnum,
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
)]
#[strum(serialize_all = "lowercase")]
/// Available cgroup managers.
pub enum CgroupManager {
    /// Use systemd to create and manage cgroups
    Systemd,

    /// Use the cgroup filesystem to create and manage cgroups
    Cgroupfs,
}

impl Default for CgroupManager {
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

impl Config {
    /// Validate the configuration integrity.
    pub fn validate(&self) -> Result<()> {
        if self.runtime().as_os_str().is_empty() {
            bail!("--runtime flag not set")
        }
        if self.runtime_dir().as_os_str().is_empty() {
            bail!("--runtime-dir flag not set")
        }

        if !self.runtime().exists() {
            bail!("runtime path '{}' does not exist", self.runtime().display())
        }

        if !self.runtime_dir().exists() {
            fs::create_dir_all(self.runtime_dir())?;
        }

        if let Some(rr) = self.runtime_root() {
            if !rr.exists() {
                fs::create_dir_all(rr)?;
            } else if !rr.is_dir() {
                bail!("runtime root '{}' does not exist", rr.display())
            }
        }

        if self.socket().exists() {
            fs::remove_file(self.socket())?;
        }

        Ok(())
    }
    pub fn socket(&self) -> PathBuf {
        self.runtime_dir().join(SOCKET)
    }
    pub fn conmon_pidfile(&self) -> PathBuf {
        self.runtime_dir().join(PIDFILE)
    }
}
