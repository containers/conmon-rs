#![allow(clippy::needless_return)]
#![doc = include_str!("../../../README.md")]
#![doc = include_str!("../../../usage.md")]

pub use server::Server;
pub use version::Version;

#[macro_use]
mod macros;

mod attach;
mod capnp_util;
mod child;
mod child_reaper;
mod config;
mod container_io;
mod container_log;
mod cri_logger;
mod fd_socket;
mod init;
mod journal;
mod json_logger;
mod listener;
mod oom_watcher;
mod pause;
mod rpc;
mod server;
mod streams;
mod telemetry;
mod terminal;
mod version;
