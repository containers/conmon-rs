#![doc = include_str!("../../../README.md")]
#![doc = include_str!("../../../usage.md")]
#![deny(unsafe_code)]

pub use server::Server;
pub use version::Version;

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
mod oom_watcher;
#[allow(unsafe_code)]
mod pause;
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
