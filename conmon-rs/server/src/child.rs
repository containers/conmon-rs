//! The processes the server monitors, containers and execs.

use crate::container_io::SharedContainerIO;
use nix::unistd::Pid;
use std::path::PathBuf;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// What kind of process a child is. Execs are registered under the id of
/// their container, but only the container itself is the target of
/// container level operations like attach or log reopening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildKind {
    Container,
    Exec,
}

#[derive(Debug)]
pub struct Child {
    pub(crate) id: Box<str>,
    pub(crate) kind: ChildKind,
    pub(crate) pid: Pid,
    pub(crate) exit_paths: Vec<PathBuf>,
    pub(crate) oom_exit_paths: Vec<PathBuf>,
    pub(crate) timeout: Option<Instant>,
    pub(crate) io: SharedContainerIO,
    pub(crate) cleanup_cmd: Vec<String>,
    pub(crate) token: CancellationToken,
}

/// Sleep until the deadline, forever without one.
pub(crate) async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
