#![doc = include_str!("../../../README.md")]
#![doc = include_str!("../../../usage.md")]
#![deny(unsafe_code)]

pub use server::Server;
pub use version::Version;

#[macro_use]
mod macros;

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
mod oom_watcher;
#[allow(unsafe_code)]
mod pause;
mod rpc;
#[allow(unsafe_code)]
mod server;
mod streaming_server;
mod streams;
mod telemetry;
#[allow(unsafe_code)]
mod terminal;
mod version;
