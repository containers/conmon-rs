//! File descriptor mapping for child process spawning.
//!
//! This mirrors the API of the `command-fds` crate (`CommandFdExt`,
//! `FdMapping`, `FdMappingCollision`) without pulling in the dependency.
//!
//! On Linux, std creates a close-on-exec socket before it forks, the child
//! writes the error of a failed exec into it and the parent reads the end of
//! file on a successful exec. Its end in the child gets the lowest free
//! number, which may be the target of a mapping. Replacing it would make a
//! failed exec look like a success, and the error would go into the
//! descriptor of the mapping. So the mappings hold the free target numbers
//! with placeholders until the command gets dropped, which keeps the socket
//! off them. A target held by another descriptor of the server may still
//! become free and get the socket, another thread may close it meanwhile.
//! The child checks that it still holds the same file and fails with
//! `EBUSY` otherwise, instead of replacing what it does not know.
//!
//! Commands with mappings get built and spawned under [`spawn_lock`], so that
//! the placeholders of one do not look like changed targets to another. A
//! spawn which failed with `EBUSY` can be tried again with a new command.
//!
//! Mappings to 0, 1 and 2 replace the stdio of the command, which std sets up
//! before, they are not checked.

use crate::mutex::lock;
use nix::{
    errno::Errno,
    fcntl::{FcntlArg, FdFlag, fcntl},
    unistd::dup2_raw,
};
use std::{
    cmp::max,
    fmt, io,
    mem::MaybeUninit,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    sync::{Mutex, MutexGuard},
};
use tokio::process::Command;

/// Held while a command with mappings gets built, spawned and dropped, see
/// the module.
pub fn spawn_lock() -> MutexGuard<'static, ()> {
    static SPAWN_LOCK: Mutex<()> = Mutex::new(());
    lock(&SPAWN_LOCK)
}

/// Whether a spawn failed because a target of the mappings changed, see the
/// module.
pub fn target_changed(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EBUSY)
}

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
    #[expect(unsafe_code, reason = "a pre_exec hook")]
    fn fd_mappings(
        &mut self,
        mut mappings: Vec<FdMapping>,
    ) -> Result<&mut Self, FdMappingCollision> {
        let child_fds = validate_child_fds(&mappings)?;
        // Before std creates its socket, see the module.
        let (placeholders, checks) = reserve_targets(&mappings, &child_fds);

        // SAFETY: check_targets and map_fds will not allocate, so they are
        // safe to call from this hook. The hook owns the placeholders until
        // the command gets dropped.
        unsafe {
            self.pre_exec(move || {
                // Owned by the hook.
                let _placeholders = &placeholders;
                check_targets(&checks)?;
                map_fds(&mut mappings, &child_fds)
            });
        }

        Ok(self)
    }
}

/// A target number which the child checks, with the device and inode of the
/// file another descriptor of the server held there, none if it was free but
/// held by another descriptor when the placeholder got created.
type TargetCheck = (RawFd, Option<(libc::dev_t, libc::ino_t)>);

/// The device and inode of the file of a descriptor, which may be closed. It
/// is async signal safe, for the pre_exec hook.
#[expect(unsafe_code, reason = "fstat of a descriptor nothing owns here")]
fn file_of(fd: RawFd) -> nix::Result<(libc::dev_t, libc::ino_t)> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat only writes the buffer, a closed descriptor fails it.
    Errno::result(unsafe { libc::fstat(fd, stat.as_mut_ptr()) })?;
    // SAFETY: fstat succeeded, so it filled the buffer.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev, stat.st_ino))
}

/// Hold the free target numbers with placeholders, which get returned, and
/// note the files of the others, which the child checks. The descriptors of
/// the mappings need neither, the ones of stdio are not checked, see the
/// module.
#[expect(unsafe_code, reason = "taking a new placeholder")]
fn reserve_targets(
    mappings: &[FdMapping],
    child_fds: &[RawFd],
) -> (Vec<OwnedFd>, Vec<TargetCheck>) {
    let (mut placeholders, mut checks) = (vec![], vec![]);
    let Some(source) = mappings.first() else {
        return (placeholders, checks);
    };
    for &fd in child_fds.iter().filter(|&&fd| fd > libc::STDERR_FILENO) {
        if mappings.iter().any(|m| m.parent_fd.as_raw_fd() == fd) {
            continue;
        }
        // The lowest free number from the target on.
        if let Ok(placeholder) = fcntl(&source.parent_fd, FcntlArg::F_DUPFD_CLOEXEC(fd)) {
            // SAFETY: the duplicate was just created and nothing else owns
            // it.
            let placeholder = unsafe { OwnedFd::from_raw_fd(placeholder) };
            if placeholder.as_raw_fd() == fd {
                placeholders.push(placeholder);
                continue;
            }
        }
        checks.push((fd, file_of(fd).ok()));
    }
    (placeholders, checks)
}

/// Fail if a target number which another descriptor held in the parent holds
/// something else in the child, it may be the socket of std, see the module.
/// It must not allocate, as it is called from the pre_exec hook.
fn check_targets(checks: &[TargetCheck]) -> io::Result<()> {
    for &(fd, file) in checks {
        match file_of(fd) {
            Err(Errno::EBADF) => {}
            Ok(found) if file == Some(found) => {}
            Ok(_) => return Err(io::Error::from_raw_os_error(libc::EBUSY)),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
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
#[expect(unsafe_code, reason = "taking and leaking descriptors in the child")]
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

    /// Spawn the command with the mappings like the server does and take its
    /// output.
    async fn output(
        cmd: Command,
        mappings: Vec<FdMapping>,
    ) -> anyhow::Result<std::process::Output> {
        let child = {
            let _spawning = spawn_lock();
            // Dropped before the lock, also on a failure.
            let mut cmd = cmd;
            cmd.fd_mappings(mappings)?
                .stdout(std::process::Stdio::piped())
                .spawn()?
        };
        Ok(child.wait_with_output().await?)
    }

    /// A target held by another descriptor whose file changed before the
    /// spawn fails it, it may be the socket of std. One which got closed
    /// does not.
    #[tokio::test]
    #[expect(unsafe_code, reason = "taking duplicated descriptors")]
    async fn changed_targets_fail_the_spawn() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // A duplicate of a new file at the lowest free number from `fd` on.
        let file_from = |name, fd| -> anyhow::Result<OwnedFd> {
            let file = File::create(dir.path().join(name))?;
            let fd = fcntl(&file, FcntlArg::F_DUPFD_CLOEXEC(fd))?;
            // SAFETY: the duplicate is new and owned by nothing else.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        };
        let spawning = spawn_lock();
        let mut children = vec![];
        for change in [true, false] {
            // Well above what the test harness uses.
            let held = file_from("held", 256)?;
            let target = held.as_raw_fd();
            let mut cmd = Command::new("true");
            cmd.fd_mappings(vec![FdMapping {
                parent_fd: file_from("mapped", 0)?,
                child_fd: target,
            }])?;
            drop(held);
            let other = if change {
                let other = file_from("other", target)?;
                assert_eq!(other.as_raw_fd(), target);
                Some(other)
            } else {
                None
            };
            match cmd.spawn() {
                Err(e) => assert!(change && target_changed(&e), "{e}"),
                Ok(child) => {
                    assert!(!change);
                    children.push(child);
                }
            }
            drop((cmd, other));
        }
        drop(spawning);
        for mut child in children {
            assert!(child.wait().await?.success());
        }
        Ok(())
    }

    /// A descriptor which is the target of another mapping must be moved out
    /// of the way before it gets overwritten.
    #[tokio::test]
    #[expect(unsafe_code, reason = "taking duplicated descriptors")]
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

        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("readlink /proc/self/fd/{a_fd} /proc/self/fd/{b_fd}"),
        ]);
        let output = output(
            cmd,
            vec![
                FdMapping {
                    parent_fd: a,
                    child_fd: b_fd,
                },
                FdMapping {
                    parent_fd: b,
                    child_fd: a_fd,
                },
            ],
        )
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

    /// A free target number does not get the socket which std reports a
    /// failed exec on, the mapping would replace it, the spawn would succeed
    /// and the error go into the file of the mapping.
    #[tokio::test]
    async fn failed_execs_fail_the_spawn() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let paths = [dir.path().join("a"), dir.path().join("b")];
        let mut mappings = vec![];
        for path in &paths {
            mappings.push(FdMapping {
                parent_fd: File::create(path)?.into(),
                child_fd: 0,
            });
        }
        // The lowest free numbers, unless another test takes them meanwhile.
        // The socket is a pair, the child keeps the second one.
        let free = [File::open("/dev/null")?, File::open("/dev/null")?];
        for (mapping, file) in mappings.iter_mut().zip(&free) {
            mapping.child_fd = file.as_raw_fd();
        }
        drop(free);

        let _spawning = spawn_lock();
        let mut cmd = Command::new("/does/not/exist");
        cmd.fd_mappings(mappings)?;
        assert!(cmd.spawn().is_err());
        for path in paths {
            assert!(std::fs::read(path)?.is_empty());
        }
        Ok(())
    }

    /// A descriptor which already has the number of its mapping only loses
    /// its close-on-exec flag.
    #[tokio::test]
    async fn identical_fds_are_inherited() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a");
        let fd: OwnedFd = File::create(&path)?.into();
        let raw = fd.as_raw_fd();
        assert!(
            FdFlag::from_bits_truncate(fcntl(&fd, FcntlArg::F_GETFD)?).contains(FdFlag::FD_CLOEXEC)
        );

        let mut cmd = Command::new("readlink");
        cmd.arg(format!("/proc/self/fd/{raw}"));
        let output = output(
            cmd,
            vec![FdMapping {
                parent_fd: fd,
                child_fd: raw,
            }],
        )
        .await?;
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout)?.trim(),
            path.to_str().unwrap()
        );
        Ok(())
    }
}
