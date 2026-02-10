use crate::container_io::SharedContainerIO;
use std::path::PathBuf;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct Child {
    pub(crate) id: Box<str>,
    pub(crate) pid: u32,
    pub(crate) exit_paths: Vec<PathBuf>,
    pub(crate) oom_exit_paths: Vec<PathBuf>,
    pub(crate) timeout: Option<Instant>,
    pub(crate) io: SharedContainerIO,
    pub(crate) cleanup_cmd: Vec<String>,
    pub(crate) token: CancellationToken,
}

impl Child {
    #![allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<Box<str>>,
        pid: u32,
        exit_paths: Vec<PathBuf>,
        oom_exit_paths: Vec<PathBuf>,
        timeout: Option<Instant>,
        io: SharedContainerIO,
        cleanup_cmd: Vec<String>,
        token: CancellationToken,
    ) -> Self {
        Self {
            id: id.into(),
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
