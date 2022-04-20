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
    timeout: Option<Instant>,

    #[getset(get = "pub")]
    io: SharedContainerIO,
}

impl Child {
    pub fn new(
        id: String,
        pid: u32,
        exit_paths: Vec<PathBuf>,
        timeout: Option<Instant>,
        io: SharedContainerIO,
    ) -> Self {
        Self {
            id,
            pid,
            exit_paths,
            timeout,
            io,
        }
    }
}
