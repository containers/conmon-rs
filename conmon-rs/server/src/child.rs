use crate::cri_logger::SharedCriLogger;
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
    cri_logger: SharedCriLogger,
}

impl Child {
    pub fn new(
        id: String,
        pid: u32,
        exit_paths: Vec<PathBuf>,
        cri_logger: SharedCriLogger,
    ) -> Self {
        Self {
            id,
            pid,
            exit_paths,
            cri_logger,
        }
    }
}
