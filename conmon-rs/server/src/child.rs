//! The processes the server monitors, containers and execs.

use crate::{container_io::SharedContainerIO, process::Process};
use nix::unistd::Pid;
use std::{path::PathBuf, sync::Arc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// What kind of process a child is. Execs are registered under the id of
/// their container, but only the container itself is the target of
/// container level operations like attach or log reopening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChildKind {
    Container,
    Exec,
}

/// A container or exec, as it gets registered with the child reaper.
#[derive(Debug)]
pub(crate) struct Child {
    pub(crate) id: Box<str>,
    pub(crate) kind: ChildKind,
    pub(crate) pid: Pid,
    pub(crate) exit_paths: Vec<PathBuf>,
    pub(crate) oom_exit_paths: Vec<PathBuf>,
    pub(crate) timeout: Option<Instant>,
    pub(crate) io: SharedContainerIO,
    pub(crate) token: CancellationToken,

    /// The process, if the caller holds it already, like an exec, so that
    /// the registration does not open a second pidfd for it. The
    /// registration opens it otherwise.
    pub(crate) process: Option<Arc<Process>>,
}
