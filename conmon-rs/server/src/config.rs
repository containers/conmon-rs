//! Configuration related structures
use anyhow::{bail, Result};
use clap::{AppSettings, Parser};
use getset::{CopyGetters, Getters, Setters};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
use strum::{EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};

macro_rules! prefix {
    () => {
        "CONMON_"
    };
}

/// Specifies the full version output option.
pub const VERSION_FULL: &str = "full";

#[derive(CopyGetters, Debug, Deserialize, Eq, Getters, Parser, PartialEq, Serialize, Setters)]
#[serde(rename_all = "kebab-case")]
#[clap(
    after_help("More info at: https://github.com/containers/conmon-rs"),
    global_setting(AppSettings::NoAutoVersion)
)]

/// An OCI container runtime monitor.
pub struct Config {
    #[get = "pub"]
    #[clap(
        default_missing_value("default"),
        env(concat!(prefix!(), "VERSION")),
        long("version"),
        possible_values(["default",  VERSION_FULL]),
        short('v'),
        value_name("VERBOSITY")
    )]
    /// Show version information, specify "full" for verbose output.
    version: Option<String>,

    #[get = "pub"]
    #[clap(
        default_value("info"),
        env(concat!(prefix!(), "LOG_LEVEL")),
        long("log-level"),
        short('l'),
        possible_values(["trace", "debug", "info", "warn", "error", "off"]),
        value_name("LEVEL")
    )]
    /// The logging level of the conmon server.
    log_level: String,

    #[get = "pub"]
    #[clap(
        default_value(LogDriver::Systemd.into()),
        env(concat!(prefix!(), "LOG_DRIVERS")),
        multiple(true),
        long("log-drivers"),
        short('d'),
        possible_values(LogDriver::iter().map(|x| x.into()).collect::<Vec<&str>>()),
        value_name("DRIVERS")
    )]
    /// The logging drivers used by the server. Can be specified multiple times.
    log_drivers: Vec<LogDriver>,

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
    /// Do not fork if true
    skip_fork: bool,

    #[get_copy = "pub"]
    #[clap(
        default_value(CgroupManager::Systemd.into()),
        env(concat!(prefix!(), "CGROUP_MANAGER")),
        long("cgroup-manager"),
        short('c'),
        possible_values(CgroupManager::iter().map(|x| x.into()).collect::<Vec<&str>>()),
        value_name("MANAGER")
    )]
    /// Select the cgroup manager to be used
    cgroup_manager: CgroupManager,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    EnumIter,
    EnumString,
    Eq,
    IntoStaticStr,
    Hash,
    PartialEq,
    Serialize,
)]
#[strum(serialize_all = "lowercase")]
/// Available log drivers.
pub enum LogDriver {
    /// Log to stdout
    Stdout,

    /// Use systemd journald as log driver
    Systemd,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    EnumIter,
    EnumString,
    Eq,
    IntoStaticStr,
    Hash,
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
