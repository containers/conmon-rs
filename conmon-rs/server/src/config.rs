//! Configuration related structures
use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
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

    #[get_copy = "pub"]
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
    /// Do not fork if true
    skip_fork: bool,

    #[get_copy = "pub"]
    #[arg(
        default_value_t,
        env(concat!(prefix!(), "CGROUP_MANAGER")),
        long("cgroup-manager"),
        short('c'),
        //possible_values(CgroupManager::iter().map(|x| x.into()).collect::<Vec<&str>>()),
        value_enum,
        value_name("MANAGER")
    )]
    /// Select the cgroup manager to be used
    cgroup_manager: CgroupManager,
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

        Ok(())
    }
    pub fn socket(&self) -> PathBuf {
        self.runtime_dir().join(SOCKET)
    }
    pub fn conmon_pidfile(&self) -> PathBuf {
        self.runtime_dir().join(PIDFILE)
    }
}
