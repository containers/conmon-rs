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
    log_path: PathBuf,
}

impl Child {
    /// Createa a new child instance.
    pub fn new(id: String, pid: u32, exit_paths: Vec<PathBuf>, log_path: PathBuf) -> Self {
        Self {
            id,
            pid,
            exit_paths,
            log_path,
        }
    }
}
