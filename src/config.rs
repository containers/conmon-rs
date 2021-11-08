//! Configuration related structures
use anyhow::{bail, Result};
use clap::{crate_version, Parser};
use derive_builder::Builder;
use getset::{CopyGetters, Getters, Setters};
use serde::{Deserialize, Serialize};
use log::{LevelFilter};
use std::{env, path::PathBuf};

macro_rules! prefix {
    () => {
        "CONMON_"
    };
}

#[derive(
    Builder, CopyGetters, Debug, Deserialize, Eq, Getters, Parser, PartialEq, Serialize, Setters,
)]
#[builder(default, pattern = "owned", setter(into, strip_option))]
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
        possible_values(["trace", "debug", "info", "warn", "error", "off"]),
        value_name("LEVEL")
    )]
    /// The logging level of the conmon server.
    log_level: LevelFilter,

    #[get = "pub"]
    #[clap(
        env(concat!(prefix!(), "PIDFILE")),
        long("conmon-pidfile"),
        short('P'),
        value_name("PATH")
    )]
    /// PID file for the conmon server.
    conmon_pidfile: Option<PathBuf>,
    
    #[get = "pub"]
    #[clap(
        env(concat!(prefix!(), "RUNTIME")),
        long("runtime"),
        short('r'),
        value_name("PATH")
    )]
    /// Path of the OCI runtime to use to operate on the containers.
    runtime: PathBuf,
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

        Ok(())
    }
}

