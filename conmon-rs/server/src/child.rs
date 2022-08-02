use crate::container_io::SharedContainerIO;
use getset::{CopyGetters, Getters};
use std::path::PathBuf;
use tokio::time::Instant;

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
}

impl Child {
    pub fn new(
        id: String,
        pid: u32,
        exit_paths: Vec<PathBuf>,
        oom_exit_paths: Vec<PathBuf>,
        timeout: Option<Instant>,
        io: SharedContainerIO,
        cleanup_cmd: Vec<String>,
    ) -> Self {
        Self {
            id,
            pid,
            exit_paths,
            oom_exit_paths,
            timeout,
            io,
            cleanup_cmd,
        }
    }
}
