//! A pod level OCI container runtime monitor for Linux.
//!
//! A container engine like CRI-O starts one `conmonrs` per pod and talks to
//! it over a Cap'n Proto RPC interface on a UNIX socket, which this crate
//! serves. The crate is the server only, the Go client lives in the same
//! repository under `pkg/client`. The repository holds the full
//! documentation:
//!
//! - <https://github.com/containers/conmon-rs/blob/main/README.md> on what
//!   conmon-rs does and how it is built.
//! - <https://github.com/containers/conmon-rs/blob/main/usage.md> on the command
//!   line, the RPCs and the attach, streaming and fd socket protocols.

use anyhow::{Context, Result};
use config::{Commands, Config, Verbosity};
use pause::Pause;
use server::Server;
use version::Version;

/// Run conmonrs: print the version, run the `pause` subcommand or start the
/// server, depending on the command line.
pub fn run() -> Result<()> {
    // Before the heap grows, see the function.
    init::disable_thp();
    let mut config = Config::parse_args();

    if let Some(v) = config.version() {
        Version::new(v == Verbosity::Full).print();
        return Ok(());
    }

    if let Some(v) = config.version_json() {
        return Version::new(v == Verbosity::Full).print_json();
    }

    if let Some(Commands::Pause(args)) = config.command() {
        // The server adds stderr to the error of the request, see
        // `Pause::init`, it runs without logging. One line per process: the
        // forked child of the pause process ends up here as well and may add
        // its own failure.
        if let Err(e) = Pause::run(args) {
            eprintln!("run pause: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    config.validate().context("validate config")?;
    config.make_absolute()?;
    config.prepare().context("prepare runtime directories")?;

    Server::new(config)
        .context("create server")?
        .start()
        .context("start server")
}

/// The generated Cap'n Proto RPC interface.
pub mod conmon_capnp {
    #![allow(
        clippy::all,
        clippy::nursery,
        clippy::pedantic,
        clippy::restriction,
        unused_parens,
        reason = "generated code"
    )]
    include!(concat!(env!("OUT_DIR"), "/proto/conmon_capnp.rs"));
}

/// The entry points of the fuzz targets in `fuzz/`, not an API.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing {
    pub use crate::{
        container_log::fuzzing::lines as log_lines, journal::fuzzing::entry as journal_entry,
        notify::fuzzing::filter as notify_filter, process::fuzzing::stat as proc_stat,
        streaming_server::fuzzing::*,
    };

    #[cfg(test)]
    mod tests;
}

mod async_fd;
mod attach;
mod capnp_util;
mod child;
mod child_reaper;
mod config;
mod container_io;
mod container_log;
mod fd_mapping;
mod fd_socket;
mod fs_util;
mod init;
mod journal;
mod listener;
mod mutex;
mod notify;
mod oom_watcher;
mod pause;
mod process;
mod random_id;
mod rpc;
mod runtime_args;
mod server;
mod single_flight;
mod streaming_server;
mod streams;
mod tail;
#[cfg(feature = "tracing")]
mod telemetry;
mod terminal;
#[cfg(test)]
mod test_util;
#[cfg(all(target_os = "linux", target_arch = "s390x", target_env = "musl"))]
mod unwind;
mod version;
