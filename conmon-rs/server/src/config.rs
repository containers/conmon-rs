//! Configuration related structures
use anyhow::{bail, Result};
use clap::{crate_version, Parser};
use getset::{CopyGetters, Getters, Setters};
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use std::fs;
use std::{env, path::PathBuf};

macro_rules! prefix {
    () => {
        "CONMON_"
    };
}

#[derive(CopyGetters, Debug, Deserialize, Eq, Getters, Parser, PartialEq, Serialize, Setters)]
#[serde(rename_all = "kebab-case")]
#[clap(
    after_help("More info at: https://github.com/containers/conmon"),
    version(crate_version!()),
)]

/// An OCI container runtime monitor.
pub struct Config {
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

    #[get = "pub"]
    #[clap(
        env(concat!(prefix!(), "LOG_FILE")),
        long("log-file"),
        short('f'),
        value_name("PATH")
    )]
    /// The logging file of the conmon server.
    log_file: Option<PathBuf>,

    #[get = "pub"]
    #[clap(
        env(concat!(prefix!(), "PIDFILE")),
        long("conmon-pidfile"),
        short('P'),
        value_name("PIDFILE")
    )]
    /// PID file for the conmon server.
    conmon_pidfile: Option<PathBuf>,

    #[get = "pub"]
    #[clap(
        env(concat!(prefix!(), "RUNTIME")),
        long("runtime"),
        short('r'),
        value_name("RUNTIME")
    )]
    /// Path of the OCI runtime to use to operate on the containers.
    runtime: PathBuf,

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
        env(concat!(prefix!(), "SOCKET")),
        long("socket"),
        short('s'),
        default_value("conmon.sock"),
        value_name("SOCKET")
    )]
    /// Path of the listening socket for the server.
    socket: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self::parse()
    }
}

impl Config {
    /// Validate the configuration integrity.
    pub fn validate(&self) -> Result<()> {
        if !self.runtime().exists() {
            bail!("runtime path '{}' does not exist", self.runtime().display())
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
        if let Some(parent) = self.socket().parent() {
            fs::create_dir_all(parent)?;
        }

        Ok(())
    }
}
