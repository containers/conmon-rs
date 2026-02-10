//! File descriptor mapping for child process spawning.

use nix::{
    fcntl::{FcntlArg, FdFlag, fcntl},
    unistd::dup2_raw,
};
use std::{
    cmp::max,
    fmt, io,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
};
use tokio::process::Command;

/// A mapping from a file descriptor in the parent to a file descriptor in the child.
#[derive(Debug)]
pub struct FdMapping {
    pub parent_fd: OwnedFd,
    pub child_fd: RawFd,
}

/// Extension to add file descriptor mappings to a Command.
pub trait CommandFdExt {
    fn fd_mappings(&mut self, mappings: Vec<FdMapping>) -> Result<&mut Self, FdMappingCollision>;
}

/// Error when two or more mappings target the same child FD.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FdMappingCollision;

impl fmt::Display for FdMappingCollision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Two or more mappings for the same child FD")
    }
}

impl std::error::Error for FdMappingCollision {}

impl CommandFdExt for Command {
    fn fd_mappings(
        &mut self,
        mut mappings: Vec<FdMapping>,
    ) -> Result<&mut Self, FdMappingCollision> {
        let child_fds = validate_child_fds(&mappings)?;

        // Safety: map_fds will not allocate, so it is safe to call from this hook.
        unsafe {
            self.pre_exec(move || map_fds(&mut mappings, &child_fds));
        }

        Ok(self)
    }
}

fn validate_child_fds(mappings: &[FdMapping]) -> Result<Vec<RawFd>, FdMappingCollision> {
    let mut child_fds: Vec<RawFd> = mappings.iter().map(|m| m.child_fd).collect();
    child_fds.sort_unstable();
    child_fds.dedup();
    if child_fds.len() != mappings.len() {
        return Err(FdMappingCollision);
    }
    Ok(child_fds)
}

// This function must not allocate, as it is called from the pre_exec hook.
fn map_fds(mappings: &mut [FdMapping], child_fds: &[RawFd]) -> io::Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }

    let first_safe_fd = mappings
        .iter()
        .map(|m| max(m.parent_fd.as_raw_fd(), m.child_fd))
        .max()
        .expect("mappings is non-empty")
        + 1;

    for mapping in mappings.iter_mut() {
        if child_fds.contains(&mapping.parent_fd.as_raw_fd())
            && mapping.parent_fd.as_raw_fd() != mapping.child_fd
        {
            let parent_fd = fcntl(&mapping.parent_fd, FcntlArg::F_DUPFD_CLOEXEC(first_safe_fd))?;
            unsafe {
                mapping.parent_fd = OwnedFd::from_raw_fd(parent_fd);
            }
        }
    }

    for mapping in mappings {
        if mapping.child_fd == mapping.parent_fd.as_raw_fd() {
            fcntl(&mapping.parent_fd, FcntlArg::F_SETFD(FdFlag::empty()))?;
        } else {
            unsafe {
                let _ = dup2_raw(&mapping.parent_fd, mapping.child_fd)?.into_raw_fd();
            }
        }
    }

    Ok(())
}
