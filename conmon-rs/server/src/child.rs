use crate::container_log::SharedContainerLog;
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
}

impl Child {
    pub fn new(id: String, pid: u32, exit_paths: Vec<PathBuf>, logger: SharedContainerLog) -> Self {
        Self {
            id,
            pid,
            exit_paths,
            logger,
        }
    }
}
