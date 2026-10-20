//! Signalling child processes without hitting another process which reused
//! their PID.

use nix::{
    errno::Errno,
    sys::{
        signal::{Signal, kill},
        wait::{Id, WaitPidFlag, waitid},
    },
    unistd::Pid,
};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use tracing::debug;

/// Open a pidfd for the process, which also works for one that already
/// exited but was not reaped yet. The descriptor is close-on-exec.
pub fn pidfd_open(pid: Pid) -> nix::Result<OwnedFd> {
    // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor
    // or a negative error, it does not touch any memory of ours.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.as_raw(), 0) };
    Errno::result(fd)?;
    // SAFETY: the descriptor was just returned by the kernel and is not
    // owned by anything else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// Send a signal through a pidfd, none only checks that the process still
/// exists.
pub fn pidfd_send_signal(pidfd: impl AsFd, signal: Option<Signal>) -> nix::Result<()> {
    let signal = signal.map_or(0, |signal| signal as libc::c_int);
    // SAFETY: the descriptor is open for the duration of the call, the null
    // siginfo makes the kernel fill in the same as kill(2) does.
    let res = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_fd().as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    Errno::result(res).map(drop)
}

/// A child process of the server, which can be signalled without hitting
/// another process that reused its PID.
#[derive(Debug)]
pub struct Process {
    pid: Pid,

    /// Keeps referring to the process after it got reaped, unlike the PID.
    /// None if the kernel does not support pidfds.
    pub(crate) pidfd: Option<OwnedFd>,
}

impl Process {
    /// Refer to a child which was not reaped yet, so the PID is still its
    /// own.
    pub fn open(pid: Pid) -> Self {
        let pidfd = match pidfd_open(pid) {
            Ok(pidfd) => Some(pidfd),
            Err(e) => {
                debug!("No pidfd for PID {pid}, checking the PID instead: {e}");
                None
            }
        };
        Self { pid, pidfd }
    }

    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Signal the process, none only checks that it still exists. Its PID
    /// gets reused once something took its exit status, the pidfd still
    /// refers to the exited process then and the signal fails with `ESRCH`
    /// instead of hitting another process.
    pub fn signal(&self, signal: impl Into<Option<Signal>>) -> nix::Result<()> {
        let signal = signal.into();
        if let Some(pidfd) = &self.pidfd {
            return pidfd_send_signal(pidfd, signal);
        }

        // Without pidfds (Linux before 5.3) check that the process is still a
        // child of this server which was not reaped yet, its PID cannot be
        // reused then. Only a reap between the check and the signal is left.
        waitid(
            Id::Pid(self.pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
        )?;
        kill(self.pid, signal)
    }
}
