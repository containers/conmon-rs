//! A pod level OCI container runtime monitor for Linux.
//!
//! A container engine like CRI-O starts one `conmonrs` per pod and talks to
//! it over a Cap'n Proto RPC interface on a UNIX socket, which this crate
//! serves. The crate is the server only, the Go client lives in the same
//! repository under `pkg/client`. The repository holds the full
//! documentation:
//!
//! - <https://github.com/containers/conmon-rs/blob/main/README.md> on what
//!   conmon-rs does.
//! - <https://github.com/containers/conmon-rs/blob/main/docs/architecture.md>
//!   on how the server and the client work together.
//! - <https://github.com/containers/conmon-rs/blob/main/docs/install.md> on
//!   how to install, verify and build it.
//! - <https://github.com/containers/conmon-rs/blob/main/docs/server.md> on the
//!   command line.
//! - <https://github.com/containers/conmon-rs/blob/main/docs/protocols.md> on
//!   the RPCs and the attach and streaming protocols.
//! - <https://github.com/containers/conmon-rs/blob/main/README.md#documentation>
//!   on the other documents, like configuring CRI-O, logging and
//!   troubleshooting.

use anyhow::{Context, Result};
use config::{Config, Verbosity};
use server::Server;
use version::Version;

/// Run conmonrs: print the version or start the server, depending on the
/// command line.
pub fn run() -> Result<()> {
    // Before the heap grows, see the function.
    init::disable_thp();
    let mut config = Config::parse_args();

    if let Some(v) = config.version() {
        return Version::new(v == Verbosity::Full).print();
    }

    if let Some(v) = config.version_json() {
        return Version::new(v == Verbosity::Full).print_json();
    }

    config.validate().context("validate config")?;
    config.make_absolute()?;
    // Before the runtime directories get created, they would get the umask
    // of the caller otherwise.
    init::set_default_umask();
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
        oom_watcher::fuzzing::cgroup_files, process::fuzzing::stat as proc_stat,
        streaming_server::fuzzing::*,
    };

    #[cfg(test)]
    mod tests;
}

mod async_fd;
mod attach;
mod capnp_util;
mod channel;
mod child_reaper;
mod config;
mod container_io;
mod container_log;
mod fs_util;
mod init;
mod journal;
mod listener;
mod mutex;
mod oom_watcher;
mod process;
mod random_id;
mod rpc;
mod runtime_args;
mod server;
mod streaming_server;
mod tail;
#[cfg(feature = "tracing")]
mod telemetry;
mod terminal;
#[cfg(test)]
mod test_util;
#[cfg(all(target_os = "linux", target_arch = "s390x", target_env = "musl"))]
mod unwind;
mod util;
mod version;
