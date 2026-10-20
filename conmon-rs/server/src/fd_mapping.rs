//! File descriptor mapping for child process spawning.
//!
//! This mirrors the API of the `command-fds` crate (`CommandFdExt`,
//! `FdMapping`, `FdMappingCollision`) without pulling in the dependency.

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
            // SAFETY: the duplicate was just created and nothing else owns it,
            // the old descriptor gets closed when the mapping is overwritten.
            unsafe {
                mapping.parent_fd = OwnedFd::from_raw_fd(parent_fd);
            }
        }
    }

    for mapping in mappings {
        if mapping.child_fd == mapping.parent_fd.as_raw_fd() {
            fcntl(&mapping.parent_fd, FcntlArg::F_SETFD(FdFlag::empty()))?;
        } else {
            // SAFETY: the child descriptor number is not owned by anything
            // else, the descriptors below first_safe_fd are the ones mapped
            // here, and the result is leaked on purpose for the exec.
            unsafe {
                let _ = dup2_raw(&mapping.parent_fd, mapping.child_fd)?.into_raw_fd();
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn mapping(child_fd: RawFd) -> Result<FdMapping, std::io::Error> {
        Ok(FdMapping {
            parent_fd: File::open("/dev/null")?.into(),
            child_fd,
        })
    }

    #[test]
    fn child_fds_must_be_unique() -> Result<(), std::io::Error> {
        let mappings = [mapping(3)?, mapping(4)?];
        assert_eq!(validate_child_fds(&mappings).unwrap(), vec![3, 4]);

        let mappings = [mapping(3)?, mapping(3)?];
        assert!(validate_child_fds(&mappings).is_err());
        Ok(())
    }

    /// A descriptor which is the target of another mapping must be moved out
    /// of the way before it gets overwritten.
    #[tokio::test]
    async fn swapped_fds_are_mapped() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (a_path, b_path) = (dir.path().join("a"), dir.path().join("b"));
        // Free numbers well above what the test harness uses.
        let a: OwnedFd = File::create(&a_path)?.into();
        let a = fcntl(&a, FcntlArg::F_DUPFD_CLOEXEC(200))?;
        // SAFETY: the duplicate is new and owned by nothing else.
        let a = unsafe { OwnedFd::from_raw_fd(a) };
        let b: OwnedFd = File::create(&b_path)?.into();
        let b = fcntl(&b, FcntlArg::F_DUPFD_CLOEXEC(200))?;
        // SAFETY: as above.
        let b = unsafe { OwnedFd::from_raw_fd(b) };
        let (a_fd, b_fd) = (a.as_raw_fd(), b.as_raw_fd());

        let output = Command::new("sh")
            .args([
                "-c",
                &format!("readlink /proc/self/fd/{a_fd} /proc/self/fd/{b_fd}"),
            ])
            .fd_mappings(vec![
                FdMapping {
                    parent_fd: a,
                    child_fd: b_fd,
                },
                FdMapping {
                    parent_fd: b,
                    child_fd: a_fd,
                },
            ])?
            .output()
            .await?;
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout)?;
        let links: Vec<_> = stdout.lines().collect();
        assert_eq!(
            links,
            [b_path.to_str(), a_path.to_str()].map(Option::unwrap)
        );
        Ok(())
    }
}
