#![allow(clippy::needless_return)]
#![doc = include_str!("../../../README.md")]
#![doc = include_str!("../../../usage.md")]

pub use server::Server;
pub use version::Version;

#[macro_use]
mod macros;

mod attach;
mod bounded_hashmap;
mod capnp_util;
mod child;
mod child_reaper;
mod config;
mod container_io;
mod container_log;
mod fd_mapping;
mod fd_socket;
mod init;
mod journal;
mod listener;
mod oom_watcher;
mod pause;
mod rpc;
mod server;
mod streaming_server;
mod streams;
mod telemetry;
mod terminal;
mod version;
