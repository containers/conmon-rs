//! Configuration related structures
use anyhow::{bail, Context, Result};
use clap::{AppSettings, Parser};
use getset::{CopyGetters, Getters, Setters};
use log::{debug, LevelFilter};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
};
use strum::{EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};

macro_rules! prefix {
    () => {
        "CONMON_"
    };
}

#[derive(CopyGetters, Debug, Deserialize, Eq, Getters, Parser, PartialEq, Serialize, Setters)]
#[serde(rename_all = "kebab-case")]
#[clap(
    after_help("More info at: https://github.com/containers/conmon-rs"),
    global_setting(AppSettings::NoAutoVersion)
)]

/// An OCI container runtime monitor.
pub struct Config {
    #[get_copy = "pub"]
    #[clap(long("version"), short('v'))]
    /// Show version information.
    version: bool,

    #[get_copy = "pub"]
    #[clap(
        default_value("info"),
        env(concat!(prefix!(), "LOG_LEVEL")),
        long("log-level"),
        short('l'),
        possible_values(["trace", "debug", "info", "warn", "error", "off"]),
        value_name("LEVEL")
    )]
    /// The logging level of the conmon server.
    log_level: LevelFilter,

    #[get_copy = "pub"]
    #[clap(
        default_value(LogDriver::Stdout.into()),
        env(concat!(prefix!(), "LOG_DRIVER")),
        long("log-driver"),
        short('d'),
        possible_values(LogDriver::iter().map(|x| x.into()).collect::<Vec<&str>>()),
        value_name("DRIVER")
    )]
    /// The logging driver used by the conmon server.
    log_driver: LogDriver,

    #[get = "pub"]
    #[clap(
        default_value(DEFAULT_RUNTIME),
        env(concat!(prefix!(), "RUNTIME")),
        long("runtime"),
        short('r'),
        value_name("RUNTIME")
    )]
    /// Path of the OCI runtime to use to operate on the containers.
    runtime: PathBuf,

    #[get = "pub"]
    #[clap(
        default_value_if("version", None, Some("")),
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
    /// Path of the OCI runtime to use to operate on the containers.
    runtime_root: Option<PathBuf>,

    #[get = "pub"]
    #[clap(
        env(concat!(prefix!(), "SKIP_FORK")),
        long("skip-fork"),
        value_name("SKIP_FORK")
    )]
    /// Do not fork if true
    skip_fork: bool,
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

impl Default for Config {
    fn default() -> Self {
        Self::parse()
    }
}

// Sync with `pkg/client/client.go`
const SOCKET: &str = "conmon.sock";
const PIDFILE: &str = "pidfile";

const DEFAULT_RUNTIME: &str = "runc";

impl Config {
    /// Validate the configuration integrity.
    pub fn validate(&mut self) -> Result<()> {
        if self.runtime() == Path::new(DEFAULT_RUNTIME) {
            self.runtime = Self::find_in_path(DEFAULT_RUNTIME).context(format!(
                "find default runtime '{}' in $PATH",
                DEFAULT_RUNTIME
            ))?;
            debug!("Using runtime: {}", self.runtime().display());
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

    fn find_in_path<P>(name: P) -> Option<PathBuf>
    where
        P: AsRef<Path>,
    {
        env::var_os("PATH").and_then(|p| {
            env::split_paths(&p).find_map(|dir| {
                let path = dir.join(&name);
                if path.is_file() {
                    Some(path)
                } else {
                    None
                }
            })
        })
    }
}
