//! Signalling child processes without hitting another process which reused
//! their PID.

use anyhow::{Context, Result, bail};
use nix::{
    errno::Errno,
    sys::{
        signal::{Signal, kill},
        wait::{Id, WaitPidFlag, WaitStatus, waitid},
    },
    unistd::{Pid, getpgid, getpid},
};
use std::{
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
    sync::{PoisonError, RwLock, RwLockWriteGuard},
};
use tracing::debug;

/// Read a PID file like the runtimes write it, the number may be followed by
/// a line ending.
pub fn read_pid_file(path: &Path) -> Result<Pid> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read PID file {}", path.display()))?;
    parse_pid(&content).with_context(|| format!("parse PID file {}", path.display()))
}

/// Parse a PID, which is positive.
fn parse_pid(pid: &str) -> Result<Pid> {
    positive_pid(pid.trim().parse()?)
}

/// The PID of a spawned child by the ID which the standard library reports,
/// or tokio, which has none anymore once the child got reaped.
pub fn child_pid(id: impl Into<Option<u32>>) -> Result<Pid> {
    let id = id.into().context("child got reaped already")?;
    positive_pid(i32::try_from(id).with_context(|| format!("invalid PID {id}"))?)
}

/// A PID, which is positive. Zero and negative numbers refer to process
/// groups when signalling.
fn positive_pid(pid: i32) -> Result<Pid> {
    if pid <= 0 {
        bail!("invalid PID {pid}")
    }
    Ok(Pid::from_raw(pid))
}

/// Taken for writing while the server reaps one of its children, and for
/// reading while it checks that one was not reaped yet and signals it. A
/// child found unreaped under the read lock stays unreaped until the signals
/// were sent. So neither its PID nor the ID of its process group, which it
/// keeps referring to as a zombie, can be taken by another process meanwhile.
/// The server is the only one which reaps its children.
static REAP_LOCK: RwLock<()> = RwLock::new(());

/// Hold while reaping a child of the server, see [`REAP_LOCK`].
pub fn reap_guard() -> RwLockWriteGuard<'static, ()> {
    REAP_LOCK.write().unwrap_or_else(PoisonError::into_inner)
}

/// The fields of `/proc/<pid>/stat` which follow the parenthesized command
/// name, which may contain spaces and parentheses itself.
#[derive(Debug, PartialEq)]
struct Stat {
    state: char,
    ppid: i32,
    pgrp: i32,
}

impl Stat {
    fn read(pid: Pid) -> Option<Self> {
        Self::parse(&std::fs::read(format!("/proc/{pid}/stat")).ok()?)
    }

    /// The command name is not UTF-8 necessarily, any process can set its
    /// own, only the fields after it are.
    fn parse(stat: &[u8]) -> Option<Self> {
        let end = stat.windows(2).rposition(|w| w == b") ")?;
        let rest = std::str::from_utf8(&stat[end + 2..]).ok()?;
        let mut fields = rest.split_whitespace();
        Some(Self {
            state: fields.next()?.chars().next()?,
            ppid: fields.next()?.parse().ok()?,
            pgrp: fields.next()?.parse().ok()?,
        })
    }
}

/// The PIDs of all processes, without the ones of their threads.
fn pids() -> impl Iterator<Item = Pid> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| parse_pid(entry.file_name().to_str()?).ok())
}

/// Whether the process is a zombie.
pub fn is_zombie(pid: Pid) -> bool {
    Stat::read(pid).is_some_and(|stat| stat.state == 'Z')
}

/// The children of this process, found by scanning all processes.
pub fn children_by_scan() -> Vec<Pid> {
    let own = getpid().as_raw();
    pids()
        .filter(|pid| Stat::read(*pid).is_some_and(|stat| stat.ppid == own))
        .collect()
}

/// Whether a child of this process belongs to the process group.
fn group_has_child(pgid: Pid) -> bool {
    let own = getpid().as_raw();
    pids()
        .filter_map(Stat::read)
        .any(|stat| stat.ppid == own && stat.pgrp == pgid.as_raw())
}

/// Whether the process group of a child gets signalled. Group 1 would end up
/// as kill(-1), which kills everything conmon-rs is allowed to, and the group
/// of this server would kill it as well. A container process which did not
/// get a group of its own shares the one of its runtime, which runs in a
/// group of its own.
pub fn signals_group(pgid: Pid, own_pgid: Option<Pid>) -> bool {
    pgid.as_raw() > 1 && Some(pgid) != own_pgid
}

/// Open a pidfd for the process, which also works for one that already
/// exited but was not reaped yet. The descriptor is close-on-exec.
#[expect(unsafe_code, reason = "a system call nix does not wrap")]
pub fn pidfd_open(pid: Pid) -> nix::Result<OwnedFd> {
    // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor
    // or a negative error, it does not touch any memory of ours.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.as_raw(), 0) };
    let fd = RawFd::try_from(Errno::result(fd)?).map_err(|_| Errno::EBADF)?;
    // SAFETY: the descriptor was just returned by the kernel and is not
    // owned by anything else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Send a signal through a pidfd, none only checks that the process still
/// exists.
#[expect(unsafe_code, reason = "a system call nix does not wrap")]
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

    /// Refer to a child by its PID alone, which gets checked to still belong
    /// to an unreaped child of the server before every signal.
    pub fn child(pid: Pid) -> Self {
        Self { pid, pidfd: None }
    }

    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Signal the process, none only checks that it still exists. Its PID
    /// gets reused once something took its exit status, the pidfd still
    /// refers to the exited process then and the signal fails with `ESRCH`
    /// instead of hitting another process.
    pub fn signal(&self, signal: impl Into<Option<Signal>>) -> nix::Result<()> {
        let _guard = REAP_LOCK.read().unwrap_or_else(PoisonError::into_inner);
        self.signal_unlocked(signal.into())
    }

    /// Whether the child still runs. A zombie which was not reaped yet does
    /// not, although it can still be signalled.
    pub fn is_running(&self) -> bool {
        let _guard = REAP_LOCK.read().unwrap_or_else(PoisonError::into_inner);
        self.signal_unlocked(None).is_ok()
            && matches!(
                waitid(
                    Id::Pid(self.pid),
                    WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
                ),
                Ok(WaitStatus::StillAlive)
            )
    }

    /// Signal the process group of the process and the process itself, once
    /// it was checked to be unreaped, see [`REAP_LOCK`]. The group only gets
    /// signalled if [`signals_group`] allows it.
    ///
    /// A process which got reaped already may have led a group which still
    /// has members, like the init of a container without a PID namespace
    /// whose children hold the pipes. They got reparented to this server,
    /// the subreaper, so the group gets signalled as long as one of them is
    /// an unreaped child of the server, which keeps the group ID in use.
    pub fn signal_group(&self, signal: Signal) -> nix::Result<()> {
        let _guard = REAP_LOCK.read().unwrap_or_else(PoisonError::into_inner);
        let own_pgid = getpgid(None).ok();
        if let Err(e) = self.signal_unlocked(None) {
            if signals_group(self.pid, own_pgid) && group_has_child(self.pid) {
                debug!("Signalling the remaining group of PID {}", self.pid);
                return kill(Pid::from_raw(-self.pid.as_raw()), signal);
            }
            return Err(e);
        }
        if let Ok(pgid) = getpgid(Some(self.pid))
            && signals_group(pgid, own_pgid)
            && let Err(e) = kill(Pid::from_raw(-pgid.as_raw()), signal)
        {
            debug!("Unable to signal process group {pgid}: {e}");
        }
        self.signal_unlocked(Some(signal))
    }

    fn signal_unlocked(&self, signal: Option<Signal>) -> nix::Result<()> {
        if let Some(pidfd) = &self.pidfd {
            return pidfd_send_signal(pidfd, signal);
        }

        // Without pidfds (Linux before 5.3) check that the process is still a
        // child of this server which was not reaped yet, its PID cannot be
        // reused then. The caller holds the lock which keeps it unreaped.
        waitid(
            Id::Pid(self.pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
        )?;
        kill(self.pid, signal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    #[test]
    fn stat_follows_the_command_name() {
        assert_eq!(
            Stat::parse(b"42 (a) b) (c) Z 1 42 42 0 -1"),
            Some(Stat {
                state: 'Z',
                ppid: 1,
                pgrp: 42
            })
        );
        assert_eq!(Stat::parse(b"42 (a) R"), None);
        // Any process can give itself a name which is not UTF-8.
        assert_eq!(
            Stat::parse(b"42 (\xff\xfe) Z 1 7 7 0 -1"),
            Some(Stat {
                state: 'Z',
                ppid: 1,
                pgrp: 7
            })
        );
    }

    #[test]
    fn pid_files_hold_positive_pids() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("pid");
        for (content, pid) in [
            ("42", Some(42)),
            ("42\n", Some(42)),
            ("0", None),
            ("-1", None),
            ("x", None),
            ("", None),
        ] {
            std::fs::write(&path, content)?;
            assert_eq!(
                read_pid_file(&path).ok(),
                pid.map(Pid::from_raw),
                "{content:?}"
            );
        }
        assert!(read_pid_file(&dir.path().join("missing")).is_err());
        Ok(())
    }

    #[test]
    fn child_ids_are_positive_pids() {
        let pid = |id: Option<u32>| child_pid(id).ok().map(Pid::as_raw);
        assert_eq!(pid(Some(42)), Some(42));
        assert_eq!(child_pid(42).ok(), Some(Pid::from_raw(42)));
        assert_eq!(pid(Some(i32::MAX as u32)), Some(i32::MAX));
        // A reaped child has none, zero and what does not fit would refer
        // to process groups.
        assert_eq!(pid(None), None);
        assert_eq!(pid(Some(0)), None);
        assert_eq!(pid(Some(i32::MAX as u32 + 1)), None);
        assert_eq!(pid(Some(u32::MAX)), None);
        for id in [None, Some(0), Some(u32::MAX)] {
            let err = child_pid(id).err().map(|e| format!("{e:#}"));
            assert!(err.is_some_and(|e| !e.is_empty()));
        }
    }

    #[test]
    fn children_are_found_by_scan() -> Result<()> {
        let mut child = std::process::Command::new("sleep").arg("60").spawn()?;
        let pid = child_pid(child.id())?;
        let found = children_by_scan().contains(&pid);
        let zombie = is_zombie(pid);
        child.kill()?;
        child.wait()?;
        assert!(found);
        assert!(!zombie);
        Ok(())
    }

    #[test]
    fn own_process_group_is_not_signalled() -> Result<()> {
        let own = getpgid(None)?;
        assert!(!signals_group(own, Some(own)));
        assert!(!signals_group(Pid::from_raw(1), Some(own)));
        assert!(!signals_group(Pid::from_raw(0), None));
        assert!(signals_group(Pid::from_raw(own.as_raw() + 1), Some(own)));
        Ok(())
    }

    #[test]
    fn signal_group_reaches_the_group_until_reaped() -> Result<()> {
        for use_pidfd in [true, false] {
            // A shell in a group of its own with a child in the same group.
            let mut child = std::process::Command::new("sh")
                .args(["-c", "sleep 60 & wait"])
                .process_group(0)
                .spawn()?;
            let pid = child_pid(child.id())?;
            let process = if use_pidfd {
                Process::open(pid)
            } else {
                Process::child(pid)
            };

            process.signal_group(Signal::SIGTERM)?;
            assert_eq!(child.wait()?.signal(), Some(Signal::SIGTERM as i32));
            // Its group is gone with it, nothing gets signalled anymore.
            assert!(process.signal_group(Signal::SIGTERM).is_err());
        }
        Ok(())
    }

    #[test]
    fn signal_group_reaches_members_of_reaped_leader() -> Result<()> {
        // The leader of a group, and a member which outlives it.
        let mut leader = std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()?;
        let pgid = child_pid(leader.id())?;
        let mut member = std::process::Command::new("sleep")
            .arg("60")
            .process_group(pgid.as_raw())
            .spawn()?;
        leader.kill()?;
        leader.wait()?;

        let process = Process::child(pgid);
        process.signal_group(Signal::SIGTERM)?;
        assert_eq!(member.wait()?.signal(), Some(Signal::SIGTERM as i32));

        // Without members, nothing gets signalled.
        assert!(process.signal_group(Signal::SIGTERM).is_err());
        Ok(())
    }
}
