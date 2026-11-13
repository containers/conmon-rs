//! Helpers shared by the tests.

use crate::{
    child_reaper::{Child, ChildKind},
    container_io::{ContainerIO, SharedContainerIO},
    container_log::ContainerLog,
    listener::SocketInUse,
    process::{Process, child_pid},
};
use anyhow::{Context, Result, ensure, format_err};
use nix::{
    fcntl::{OFlag, open},
    pty,
    sys::{
        signal::Signal,
        socket::{self, ControlMessage, MsgFlags},
        stat::Mode,
    },
    unistd::Pid,
};
use std::{
    io::{self, IoSlice, Write},
    net::Ipv4Addr,
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};
use tokio::{
    io::Interest,
    net::{TcpSocket, UnixStream},
    task,
    time::{self, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::Dispatch;
use tracing_subscriber::util::SubscriberInitExt;

/// Write an executable shell script running `script`.
///
/// A file which the multi threaded test process wrote itself may still be
/// open for writing in a child which another thread forked meanwhile, then
/// executing it fails with ETXTBSY until that child execs. A process of its
/// own writes the file instead, so that this one never has it open.
pub(crate) fn write_script(path: &Path, script: &str) -> Result<()> {
    let mut child = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .context("spawn sh")?;
    // Closing stdin ends cat, the write may fail if sh failed early, which
    // its status tells.
    let written = child
        .stdin
        .take()
        .context("stdin of sh")?
        .write_all(format!("#!/bin/sh\n{script}\n").as_bytes());
    let status = child.wait()?;
    ensure!(
        status.success() && written.is_ok(),
        "write script {}: {status}, {written:?}",
        path.display()
    );
    Ok(())
}

/// Wait until `condition` holds, checking it every 10ms for at most 5s on
/// the clock of tokio. With a paused clock, those 5s pass at once while
/// nothing else is ready to run, so it only suits conditions which tasks
/// of the test bring about, not other processes.
pub(crate) async fn wait_for(mut condition: impl FnMut() -> bool) -> Result<()> {
    time::timeout(Duration::from_secs(5), async {
        while !condition() {
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

/// Wait in real time until `condition` holds, checking it every 10ms for at
/// most 10s, without blocking the runtime of the test. Unlike [`wait_for`],
/// it also waits with a paused clock, which processes do not advance. It
/// suits conditions which other processes bring about, like an exit, which
/// take longer on a loaded machine or under emulation. It panics instead
/// of failing, so that it also fits where nothing returns an error, like
/// in a branch of a `select!`.
pub(crate) async fn wait_for_real(condition: impl Fn() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        task::spawn_blocking(|| std::thread::sleep(std::time::Duration::from_millis(10)))
            .await
            .ok();
    }
    panic!("condition never became true");
}

/// Wait in real time until `condition` holds, like [`wait_for_real`], for
/// tests without a runtime.
pub(crate) fn wait_for_real_blocking(condition: impl Fn() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("condition never became true");
}

/// Call `f` again for up to 5s while it fails because a socket is in use. A
/// process which a parallel test forks holds the sockets of this one until
/// it execs.
pub(crate) async fn retry_while_in_use<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut result = f();
    for _ in 0..500 {
        match &result {
            Err(e) if e.is::<SocketInUse>() => {
                time::sleep(Duration::from_millis(10)).await;
                result = f();
            }
            _ => break,
        }
    }
    result
}

/// A socket bound to a port which does not listen, so that connecting to
/// the port fails. It keeps the port, a closed one could be taken by
/// another test meanwhile.
pub(crate) fn closed_port() -> Result<(TcpSocket, u16)> {
    let socket = TcpSocket::new_v4()?;
    socket.bind((Ipv4Addr::LOCALHOST, 0).into())?;
    let port = socket.local_addr()?.port();
    Ok((socket, port))
}

/// Run the ignored test at `path` in a test process of its own, with `env`
/// set, which the test checks to do nothing in any other run, and with a
/// pipe as its stdin if asked for. A test which changes its process, or
/// depends on it, would affect the tests in parallel or be affected by them
/// otherwise. Fails with the status and the output of the process unless
/// the test passed, a process which a signal killed tells only there.
pub(crate) fn run_in_own_process(path: &str, env: &str, piped_stdin: bool) -> Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command.args(["--exact", path, "--ignored"]).env(env, "1");
    if piped_stdin {
        command.stdin(Stdio::piped());
    }
    let output = command.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    ensure!(
        output.status.success() && stdout.contains("1 passed"),
        "{path}: {}\nstdout:\n{stdout}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// A child which gets killed and reaped if a test fails before it did.
pub(crate) struct Guarded(pub std::process::Child);

impl Guarded {
    pub(crate) fn spawn(cmd: &mut Command) -> Result<Self> {
        Ok(Self(cmd.spawn()?))
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        // Both do nothing for a child which got waited for already.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A shell which nobody but the test or a watcher waits for. It gets killed
/// through a pidfd if a test fails before it ended, which cannot hit a
/// reused PID.
pub(crate) struct Spawned {
    pub(crate) pid: Pid,
    pub(crate) process: Process,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.process.signal(Signal::SIGKILL);
    }
}

/// Spawn a shell running `script`, see [`Spawned`].
pub(crate) fn spawn_sh(script: &str) -> Result<Spawned> {
    let pid = child_pid(Command::new("sh").args(["-c", script]).spawn()?.id())?;
    // Not reaped yet, so the PID is still its own.
    let process = Process::open(pid);
    Ok(Spawned { pid, process })
}

/// Distinct bytes, so that a lost or reordered chunk is noticed. Built from
/// whole blocks, byte by byte takes long in emulated tests. The block length
/// is a prime, so that the pattern does not line up with buffer sizes.
pub(crate) fn pattern(len: usize) -> Vec<u8> {
    let block: Vec<u8> = (0..251).collect();
    let mut pattern = block.repeat(len.div_ceil(block.len()));
    pattern.truncate(len);
    pattern
}

/// A container without a process, exit paths or a timeout, with I/O of
/// its own, with or without a terminal. The struct update syntax sets the
/// rest, like `Child { pid, ..container_child(false)? }`.
pub(crate) fn container_child(terminal: bool) -> Result<Child> {
    Ok(Child {
        id: "id".into(),
        kind: ChildKind::Container,
        pid: Pid::from_raw(0),
        exit_paths: vec![],
        oom_exit_paths: vec![],
        timeout: None,
        io: SharedContainerIO::new(ContainerIO::new(terminal, ContainerLog::new())?),
        token: CancellationToken::new(),
        process: None,
    })
}

/// Send the terminal descriptor with some data, like a runtime does.
pub(crate) async fn send_fd(stream: &UnixStream, data: &[u8], fd: BorrowedFd<'_>) -> Result<()> {
    let fds = [fd.as_raw_fd()];
    stream
        .async_io(Interest::WRITABLE, || {
            socket::sendmsg::<()>(
                stream.as_raw_fd(),
                &[IoSlice::new(data)],
                &[ControlMessage::ScmRights(&fds)],
                MsgFlags::empty(),
                None,
            )
            .map_err(io::Error::from)
        })
        .await?;
    Ok(())
}

/// The master and the slave of a pseudo terminal, see [`open_pty`].
pub(crate) struct Pty {
    pub(crate) master: OwnedFd,
    pub(crate) slave: OwnedFd,
}

/// Open a pseudo terminal whose descriptors are close-on-exec from the
/// start, unlike the ones of `openpty`. A process which a parallel test
/// forks would inherit the slave otherwise and keep the terminal open until
/// it exits, so that its master sees no hang-up meanwhile.
pub(crate) fn open_pty() -> Result<Pty> {
    let flags = OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC;
    let master = pty::posix_openpt(flags)?;
    pty::grantpt(&master)?;
    pty::unlockpt(&master)?;
    let slave = open(pty::ptsname_r(&master)?.as_str(), flags, Mode::empty())?;
    Ok(Pty {
        master: master.into(),
        slave,
    })
}

/// The log of a test, see [`Log::capture`].
#[derive(Clone, Default)]
pub(crate) struct Log(Arc<Mutex<Vec<u8>>>);

impl Log {
    /// Collect what gets logged on the current thread at the info level and
    /// above, without times and targets, until the guard gets dropped. On
    /// the current thread runtime of a test, that includes its tasks.
    pub(crate) fn capture() -> (Self, impl Sized) {
        // With a single subscriber alive, tracing caches whether a callsite
        // is enabled by the default subscriber of the thread which hits it
        // first. A test in parallel which has none would disable it for
        // this one. With another one alive, every subscriber gets asked.
        let other: Dispatch = tracing_subscriber::fmt()
            .with_writer(io::sink)
            .finish()
            .into();
        let log = Self::default();
        let writer = log.clone();
        let guard = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .set_default();
        // Creating a subscriber already rebuilds the cache, this covers a
        // callsite which another thread registered in between.
        tracing::callsite::rebuild_interest_cache();
        (log, (guard, other))
    }

    /// What got logged so far.
    pub(crate) fn contents(&self) -> Result<String> {
        let log = self.0.lock().map_err(|_| format_err!("poisoned"))?.clone();
        Ok(String::from_utf8(log)?)
    }
}

impl Write for Log {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
