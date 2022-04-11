use crate::{attach::SharedContainerAttach, container_log::SharedContainerLog};
use getset::{CopyGetters, Getters};
use std::path::PathBuf;

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
    timeout: Option<tokio::time::Instant>,

    #[getset(get = "pub")]
    attach: SharedContainerAttach,
}

impl Child {
    pub fn new(
        id: String,
        pid: u32,
        exit_paths: Vec<PathBuf>,
        logger: SharedContainerLog,
        timeout: Option<tokio::time::Instant>,
        attach: SharedContainerAttach,
    ) -> Self {
        Self {
            id,
            pid,
            exit_paths,
            logger,
            timeout,
            attach,
        }
    }
}
