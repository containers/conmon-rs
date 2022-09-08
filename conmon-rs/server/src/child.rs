use crate::container_io::SharedContainerIO;
use getset::{CopyGetters, Getters};
use std::path::PathBuf;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, CopyGetters, Getters)]
pub struct Child {
    #[getset(get = "pub")]
    id: String,

    #[getset(get_copy = "pub")]
    pid: u32,

    #[getset(get = "pub")]
    exit_paths: Vec<PathBuf>,

    #[getset(get = "pub")]
    oom_exit_paths: Vec<PathBuf>,

    #[getset(get = "pub")]
    timeout: Option<Instant>,

    #[getset(get = "pub")]
    io: SharedContainerIO,

    #[getset(get = "pub")]
    cleanup_cmd: Vec<String>,

    #[getset(get = "pub")]
    token: CancellationToken,
}

impl Child {
    #![allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        pid: u32,
        exit_paths: Vec<PathBuf>,
        oom_exit_paths: Vec<PathBuf>,
        timeout: Option<Instant>,
        io: SharedContainerIO,
        cleanup_cmd: Vec<String>,
        token: CancellationToken,
    ) -> Self {
        Self {
            id,
            pid,
            exit_paths,
            oom_exit_paths,
            timeout,
            io,
            cleanup_cmd,
            token,
        }
    }
}
