#[derive(Debug)]
pub struct Child {
    pub id: String,
    pub pid: i32,
    pub exit_paths: Vec<std::path::PathBuf>,
}

impl Child {
    pub fn new(id: String, pid: i32, exit_paths: Vec<std::path::PathBuf>) -> Self {
        Self {
            id,
            pid,
            exit_paths,
        }
    }
}
