use crate::{
    attach::SharedContainerAttach, container_io::SharedContainerIO,
    container_log::SharedContainerLog,
};
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
    logger: SharedContainerLog,

    #[getset(get = "pub")]
    timeout: Option<Instant>,

    #[getset(get = "pub")]
    attach: SharedContainerAttach,

    #[getset(get = "pub")]
    io: SharedContainerIO,
}

impl Child {
    pub fn new(
        id: String,
        pid: u32,
        exit_paths: Vec<PathBuf>,
        logger: SharedContainerLog,
        timeout: Option<Instant>,
        attach: SharedContainerAttach,
        io: SharedContainerIO,
    ) -> Self {
        Self {
            id,
            pid,
            exit_paths,
            logger,
            timeout,
            attach,
            io,
        }
    }
}
