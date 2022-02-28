use std::path::PathBuf;

#[derive(Debug)]
pub struct Child {
    pub id: String,
    pub pid: u32,
    pub exit_paths: Vec<PathBuf>,
}

impl Child {
    pub fn new(id: String, pid: u32, exit_paths: Vec<PathBuf>) -> Self {
        Self {
            id,
            pid,
            exit_paths,
        }
    }
}
