//! A pod level OCI container runtime monitor for Linux.
//!
//! A container engine like CRI-O starts one `conmonrs` per pod and talks to
//! it over a Cap'n Proto RPC interface on a UNIX socket, which this crate
//! serves. The crate is the server only, the Go client lives in the same
//! repository under `pkg/client`. The repository holds the full documentation
//! for this version:
#![doc = concat!(
    "\n- <https://github.com/containers/conmon-rs/blob/v",
    env!("CARGO_PKG_VERSION"),
    "/README.md> on what conmon-rs does and how it is built.\n",
    "- <https://github.com/containers/conmon-rs/blob/v",
    env!("CARGO_PKG_VERSION"),
    "/usage.md> on the command line, the RPCs and the attach, streaming and fd ",
    "socket protocols.\n"
)]
#![deny(unsafe_code)]

use anyhow::{Context, Result};
use config::{Commands, Config, Verbosity};
use pause::Pause;
use server::Server;
use version::Version;

/// Run conmonrs: print the version, run the `pause` subcommand or start the
/// server, depending on the command line.
pub fn run() -> Result<()> {
    let config = Config::parse_args();

    if let Some(v) = config.version() {
        Version::new(v == Verbosity::Full).print();
        return Ok(());
    }

    if let Some(v) = config.version_json() {
        return Version::new(v == Verbosity::Full).print_json();
    }

    if let Some(Commands::Pause {
        base_path,
        pod_id,
        ipc,
        pid,
        net,
        user,
        uts,
        uid_mappings,
        gid_mappings,
    }) = config.command()
    {
        return Pause::run(
            base_path,
            pod_id,
            *ipc,
            *pid,
            *net,
            *user,
            *uts,
            uid_mappings,
            gid_mappings,
        )
        .context("run pause");
    }

    config.validate().context("validate config")?;
    config.prepare().context("prepare runtime directories")?;

    Server::new(config)
        .context("create server")?
        .start()
        .context("start server")
}

/// The generated Cap'n Proto RPC interface.
pub mod conmon_capnp {
    #![allow(clippy::all, unused_parens)]
    include!(concat!(env!("OUT_DIR"), "/proto/conmon_capnp.rs"));
}

mod attach;
mod bounded_hashmap;
mod capnp_util;
mod child;
#[allow(unsafe_code)]
mod child_reaper;
mod config;
mod container_io;
mod container_log;
#[allow(unsafe_code)]
mod fd_mapping;
mod fd_socket;
#[allow(unsafe_code)]
mod init;
mod journal;
mod listener;
mod mutex;
mod notify;
mod oom_watcher;
#[allow(unsafe_code)]
mod pause;
#[allow(unsafe_code)]
mod process;
mod rpc;
#[allow(unsafe_code)]
mod server;
mod streaming_server;
mod streams;
#[cfg(feature = "tracing")]
mod telemetry;
#[allow(unsafe_code)]
mod terminal;
#[cfg(all(target_os = "linux", target_arch = "s390x", target_env = "musl"))]
#[allow(unsafe_code)]
mod unwind;
mod version;
