//! Signalling, waiting for and reaping child processes without hitting
//! another process which reused their PID.

use crate::{
    async_fd,
    mutex::{self, lock},
};
use anyhow::{Context, Result, bail};
use nix::{
    errno::Errno,
    sys::{
        signal::{Signal, kill},
        wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid},
    },
    unistd::{Pid, getpgid, getpid},
};
use std::{
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
    sync::{Arc, LazyLock, Mutex, RwLock, RwLockWriteGuard},
    thread,
};
use tokio::{io::Interest, sync::oneshot, task};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

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
    mutex::write(&REAP_LOCK)
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

/// Whether the kernel lists the children of a thread, see [`children`].
static PROC_CHILDREN: LazyLock<bool> =
    LazyLock::new(|| Path::new("/proc/thread-self/children").exists());

/// Whether a child of this process belongs to the process group. The
/// children the kernel lists come first, only without a member among them
/// all processes get scanned: the lists are reliable for stopped children
/// only. A listed child may have got reaped meanwhile, tokio does not take
/// the [`REAP_LOCK`], so its parent gets checked again.
fn group_has_child(pgid: Pid) -> bool {
    let own = getpid().as_raw();
    let member = |pids: Vec<Pid>| {
        pids.into_iter()
            .filter_map(Stat::read)
            .any(|stat| stat.ppid == own && stat.pgrp == pgid.as_raw())
    };
    // Without the lists, children() scanned already.
    member(children().0) || *PROC_CHILDREN && member(children_by_scan())
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
    pidfd: Option<OwnedFd>,
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
        let _guard = mutex::read(&REAP_LOCK);
        self.signal_unlocked(signal.into())
    }

    /// Whether the child still runs. A zombie which was not reaped yet does
    /// not, although it can still be signalled.
    pub fn is_running(&self) -> bool {
        let _guard = mutex::read(&REAP_LOCK);
        self.signal_unlocked(None).is_ok()
            && matches!(peek_exit(Id::Pid(self.pid)), Ok(WaitStatus::StillAlive))
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
        let _guard = mutex::read(&REAP_LOCK);
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
        peek_exit(Id::Pid(self.pid))?;
        kill(self.pid, signal)
    }
}

/// Whether a child exited, without waiting or reaping it. Fails with
/// `ECHILD` if there is no such child of the server, also once it got
/// reaped.
fn peek_exit(id: Id<'_>) -> nix::Result<WaitStatus> {
    waitid(
        id,
        WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
    )
}

/// Whether a child of any thread of this process exited and was not
/// reaped yet, without reaping it.
pub fn has_zombie() -> bool {
    !matches!(
        peek_exit(Id::All),
        Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD)
    )
}

/// The zombie children of this process, and whether the list is
/// complete, see [`children`].
pub fn zombie_children() -> (Vec<Pid>, bool) {
    let (children, complete) = children();
    let zombies = children.into_iter().filter(|pid| is_zombie(*pid)).collect();
    (zombies, complete)
}

/// The children of all threads of this process. The kernel lists them in
/// `/proc/self/task/<tid>/children`, which needs `CONFIG_PROC_CHILDREN`,
/// fall back to scanning all processes without it. The list is not
/// complete if a thread exited while listing them.
fn children() -> (Vec<Pid>, bool) {
    if !*PROC_CHILDREN {
        return (children_by_scan(), true);
    }
    // Nothing gets reaped for children which cannot be listed, which is the
    // safe side.
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return (vec![], true);
    };
    let mut children = vec![];
    let mut complete = true;
    for task in tasks.flatten() {
        // A thread which exited meanwhile took its list along, its
        // children got another thread as parent.
        let list = match std::fs::read_to_string(task.path().join("children")) {
            Ok(list) => list,
            Err(e) => {
                if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) {
                    complete = false;
                }
                continue;
            }
        };
        children.extend(
            list.split_whitespace()
                .filter_map(|pid| pid.parse().ok())
                .map(Pid::from_raw),
        );
    }
    (children, complete)
}

/// The exit code of a process whose exit status is not known, because it did
/// neither exit nor get killed by a signal, or waitpid failed after its pidfd
/// got readable.
pub(crate) const EXIT_CODE_UNKNOWN: i32 = -1;

/// The exit code of a process whose exit status the blocking wait, used
/// without pidfds, could not get, for example because it got reaped already.
/// It is the same as
/// [`EXIT_CODE_TIMED_OUT`](crate::child_reaper::EXIT_CODE_TIMED_OUT) for
/// compatibility, the pidfd wait reports [`EXIT_CODE_UNKNOWN`] instead.
pub(crate) const EXIT_CODE_WAIT_FAILED: i32 = -3;

/// The exit code of a process which exited or got killed by a signal, the
/// latter like a shell reports it.
fn status_exit_code(status: WaitStatus) -> Option<i32> {
    match status {
        WaitStatus::Exited(_, code) => {
            debug!("Exited {code}");
            Some(code)
        }
        WaitStatus::Signaled(_, sig, _) => {
            debug!("Signaled: {sig}");
            Some(sig as i32 + 128)
        }
        _ => None,
    }
}

/// Wait for a process to exit using pidfd_open for async polling,
/// with a blocking waitpid fallback for older kernels.
pub async fn wait_for_exit_code(token: CancellationToken, pid: Pid) -> i32 {
    debug!("Waiting for exit code of pid {pid}");

    let exit_code = if let Some(code) = try_pidfd_wait(pid).await {
        code
    } else {
        debug!("pidfd not available, falling back to blocking waitpid");
        let (tx, rx) = oneshot::channel();
        spawn_waitpid_thread(move || {
            let _ = tx.send(wait_for_exit_code_blocking(pid));
        });
        rx.await.unwrap_or_else(|_| {
            error!("waitpid thread exited without a result");
            EXIT_CODE_WAIT_FAILED
        })
    };

    token.cancel();
    exit_code
}

/// Stack size of the waitpid threads, which only wait and log.
const WAITPID_STACK_SIZE: usize = 256 * 1024;

/// Run a blocking waitpid on a dedicated thread. It blocks for the lifetime
/// of the process, so it must not occupy a thread of the bounded blocking
/// pool, which serves the file I/O of all containers. If no thread can be
/// spawned, for example because of a pids limit, queue it on the pool
/// rather than failing the wait.
fn spawn_waitpid_thread<F: FnOnce() + Send + 'static>(f: F) {
    // The builder drops the closure on failure, keep it for the fallback.
    let slot = Arc::new(Mutex::new(Some(f)));
    fn run<F: FnOnce()>(slot: &Mutex<Option<F>>) {
        // Not under the lock, the closure blocks.
        let f = lock(slot).take();
        if let Some(f) = f {
            f();
        }
    }

    let thread_slot = slot.clone();
    if let Err(e) = thread::Builder::new()
        .name("waitpid".into())
        .stack_size(WAITPID_STACK_SIZE)
        .spawn(move || run(&thread_slot))
    {
        error!("Unable to spawn waitpid thread, using blocking pool: {e:#}");
        task::spawn_blocking(move || run(&slot));
    }
}

/// Try to use pidfd_open + async polling. Returns None if pidfd is unavailable.
async fn try_pidfd_wait(pid: Pid) -> Option<i32> {
    // The readiness comes from epoll and the wait uses WNOHANG, so the
    // descriptor itself does not need to be non-blocking.
    let owned_fd = match pidfd_open(pid) {
        Ok(fd) => fd,
        // Gone already, the blocking wait would only learn the same.
        Err(Errno::ESRCH) => {
            debug!("PID {pid} got reaped already");
            return Some(EXIT_CODE_UNKNOWN);
        }
        Err(_) => return None,
    };

    let async_fd = match async_fd::register(owned_fd, Interest::READABLE) {
        Ok(fd) => fd,
        Err(e) => {
            error!("Failed to create AsyncFd from pidfd: {e:#}");
            return None;
        }
    };

    if async_fd.readable().await.is_err() {
        return None;
    }

    // Through the pidfd, which refers to this process only: once the
    // orphan sweep took it, its PID may belong to another child already.
    // Kernels before 5.4 only know the PID.
    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG;
    let reaping = reap_guard();
    let status = match waitid(Id::PIDFd(async_fd.get_ref().as_fd()), flags) {
        Err(Errno::EINVAL) => waitpid(pid, WaitPidFlag::WNOHANG.into()),
        status => status,
    };
    drop(reaping);
    match status.map(status_exit_code) {
        Ok(Some(code)) => Some(code),
        Ok(None) => {
            debug!("Unexpected wait status after pidfd readable");
            Some(EXIT_CODE_UNKNOWN)
        }
        // The orphan sweep may have taken it meanwhile.
        Err(Errno::ECHILD) => {
            debug!("PID {pid} got reaped already");
            Some(EXIT_CODE_UNKNOWN)
        }
        Err(e) => {
            error!("waitid after pidfd readable failed: {e:#}");
            Some(EXIT_CODE_UNKNOWN)
        }
    }
}

/// Wait for the exit without reaping, which then happens under the
/// [`reap_guard`].
fn wait_for_exit_code_blocking(pid: Pid) -> i32 {
    loop {
        match waitid(Id::Pid(pid), WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT) {
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(Errno::ECHILD) => {
                debug!("PID {pid} got reaped already");
                return EXIT_CODE_WAIT_FAILED;
            }
            Err(err) => {
                error!("Unable to wait for pid {pid}: {err:#}");
                return EXIT_CODE_WAIT_FAILED;
            }
        }
        let reaping = reap_guard();
        let status = waitpid(pid, Some(WaitPidFlag::WNOHANG));
        drop(reaping);
        match status.map(status_exit_code) {
            Ok(Some(code)) => return code,
            Ok(None) => {}
            Err(Errno::EINTR) => debug!("Failed to wait for pid on EINTR, retrying"),
            Err(err) => {
                error!("Unable to waitpid on {err:#}");
                return EXIT_CODE_WAIT_FAILED;
            }
        }
    }
}

/// The entry point of the fuzz target.
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzzing;

#[cfg(test)]
mod tests;
