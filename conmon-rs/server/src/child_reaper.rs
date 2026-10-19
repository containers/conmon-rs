//! Child process reaping and management.
use crate::fd_mapping::{CommandFdExt, FdMapping};
use crate::{
    child::{Child, ChildKind},
    container_io::{ContainerIO, ContainerIOType, SharedContainerIO},
    mutex::lock,
    oom_watcher::OOMWatcher,
};
use anyhow::{Context, Result, bail};
use futures::future::join_all;
use libc::pid_t;
use nix::{
    errno::Errno,
    sys::{
        signal::{Signal, kill},
        wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid},
    },
    unistd::{Pid, getpgid},
};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fmt::{Debug, Write},
    os::fd::{FromRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    process::Stdio,
    str,
    sync::{Arc, Mutex},
    thread,
};
use tokio::{
    fs,
    io::unix::AsyncFd,
    process::Command,
    select,
    sync::{
        broadcast::{self, Receiver, Sender},
        oneshot,
    },
    task::{self, JoinHandle},
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error};

#[derive(Debug, Default)]
pub struct ChildReaper {
    grandchildren: Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>>,
}

impl ChildReaper {
    pub fn grandchildren(&self) -> &Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>> {
        &self.grandchildren
    }
}

/// first usable file descriptor after stdin, stdout and stderr
const FIRST_FD_AFTER_STDIO: RawFd = 3;

impl ChildReaper {
    /// How long to wait for the console connection of a runtime which already
    /// exited successfully.
    const CONNECT_AFTER_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
}

impl ChildReaper {
    /// Retrieve the container registered under `id`. Execs share the id of
    /// their container and are skipped.
    pub fn get(&self, id: &str) -> Result<ReapableChild> {
        let lock = lock(self.grandchildren())?;
        let r = lock
            .get(id)
            .and_then(|v| v.iter().find(|c| c.kind == ChildKind::Container))
            .context("child not available")?
            .clone();
        Ok(r)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_child<P, I, S>(
        &self,
        cmd: P,
        args: I,
        stdin: bool,
        container_io: &mut ContainerIO,
        pidfile: &Path,
        env_vars: Vec<(String, String)>,
        additional_fds: Vec<OwnedFd>,
    ) -> Result<(u32, CancellationToken)>
    where
        P: AsRef<OsStr> + Debug,
        I: IntoIterator<Item = S> + Debug,
        S: AsRef<OsStr>,
    {
        debug!("Running: {:?} {:?}", cmd, args);
        let mut cmd = Command::new(cmd);

        if stdin {
            cmd.stdin(Stdio::piped());
        }

        let mut child = cmd
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(env_vars)
            .fd_mappings(
                additional_fds
                    .into_iter()
                    .enumerate()
                    .map(|(i, fd)| FdMapping {
                        parent_fd: fd,
                        child_fd: i as RawFd + FIRST_FD_AFTER_STDIO,
                    })
                    .collect(),
            )?
            .spawn()
            .context("spawn child process")?;

        // The mappings own the passed descriptors and live in the command's
        // pre_exec hook, dropping the command closes them in the parent.
        drop(cmd);

        debug!(
            "Running child on PID: {}",
            child.id().map_or("unknown".into(), |x| x.to_string())
        );

        let token = CancellationToken::new();

        match container_io.typ_mut() {
            ContainerIOType::Terminal(terminal) => {
                // The runtime connects to the console socket before it exits.
                // If it fails before that, waiting for the connection alone
                // would hang this call forever.
                select! {
                    res = terminal.wait_connected(stdin, token.clone()) => {
                        res.context("wait for terminal socket connection")?;
                    }
                    status = child.wait() => {
                        if status.context("wait for child")?.success() {
                            // The runtime succeeded, so it did connect and the
                            // descriptor is queued already.
                            time::timeout(
                                Self::CONNECT_AFTER_EXIT_TIMEOUT,
                                terminal.wait_connected(stdin, token.clone()),
                            )
                            .await
                            .context("runtime exited without connecting the terminal")?
                            .context("wait for terminal socket connection")?;
                        }
                        // A failure is reported below together with its output.
                    }
                }
            }
            ContainerIOType::Streams(streams) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let stdin = child.stdin.take();
                streams.handle_stdio_receive(stdin, stdout, stderr, token.clone());
            }
        };

        let output = child.wait_with_output().await?;

        if !output.status.success() {
            const BASE_ERR: &str = "child command exited with";

            let mut err_str = match output.status.code() {
                Some(code) => format!("{BASE_ERR}: {code}"),
                None => format!("{BASE_ERR} signal"),
            };

            if !output.stderr.is_empty() {
                write!(err_str, ": {}", String::from_utf8_lossy(&output.stderr))?;
            }
            // token must be cancelled here because the child hasn't been registered yet,
            // meaning there is no other entity that could cancel the read_loops.
            token.cancel();

            // Do not leave a container process behind which the runtime
            // created before it failed.
            Self::cleanup_failed_grandchild(pidfile).await;

            error!("Failed: {err_str}");
            bail!(err_str)
        }

        let grandchild_pid = fs::read_to_string(pidfile)
            .await
            .with_context(|| format!("grandchild pid read error {}", pidfile.display()))?
            .parse::<u32>()
            .with_context(|| format!("grandchild pid parse error {}", pidfile.display()))?;

        Ok((grandchild_pid, token))
    }

    /// Kill and reap the container process of a failed runtime call, if the
    /// runtime got as far as writing its PID. It got reparented to this
    /// subreaper and nobody else waits for it.
    ///
    /// Only this PID is waited for. Reaping any exited child instead would take
    /// the exit status of runtimes of concurrent calls, or of grandchildren
    /// which are not registered yet.
    async fn cleanup_failed_grandchild(pidfile: &Path) {
        let Ok(content) = fs::read_to_string(pidfile).await else {
            return;
        };
        let Ok(pid) = content.trim().parse::<pid_t>() else {
            return;
        };
        let pid = Pid::from_raw(pid);

        // Make sure it is a child, the PID may have been reused otherwise.
        match waitid(
            Id::Pid(pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
        ) {
            Ok(_) => {}
            Err(e) => {
                debug!("Not cleaning up PID {pid} of failed runtime call: {e}");
                return;
            }
        }

        debug!("Cleaning up PID {pid} of failed runtime call");
        if let Err(e) = kill(pid, Signal::SIGKILL) {
            debug!("Unable to kill PID {pid}: {e}");
        }
        task::spawn(ReapableChild::wait_for_exit_code_async(
            CancellationToken::new(),
            pid.as_raw() as u32,
        ));
    }

    pub fn watch_grandchild(
        &self,
        child: Child,
        leak_fds: Vec<OwnedFd>,
    ) -> Result<Receiver<ExitChannelData>> {
        let pid = child.pid;
        let (id, mut reapable_grandchild) = ReapableChild::from_child(child);

        // Create channels outside the lock to minimize contention
        let (exit_tx, exit_rx) = reapable_grandchild.watch();

        let locked_grandchildren = self.grandchildren();
        lock(locked_grandchildren)?
            .entry(id)
            .or_default()
            .push(reapable_grandchild);
        let cleanup_grandchildren = locked_grandchildren.clone();

        task::spawn(
            async move {
                exit_tx.subscribe().recv().await?;
                drop(leak_fds);
                Self::forget_grandchild(&cleanup_grandchildren, pid)
            }
            .instrument(debug_span!("watch_grandchild", pid)),
        );
        Ok(exit_rx)
    }

    fn forget_grandchild(
        locked_grandchildren: &Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>>,
        grandchild_pid: u32,
    ) -> Result<()> {
        let mut map = lock(locked_grandchildren)?;
        for children in map.values_mut() {
            if let Some(pos) = children.iter().position(|v| v.pid == grandchild_pid) {
                children.swap_remove(pos);
                break;
            }
        }
        Ok(())
    }

    pub async fn kill_grandchildren(&self, s: Signal) -> Result<()> {
        debug!("Killing grandchildren");
        // Collect only PIDs and task handles to avoid cloning entire ReapableChild
        let children: Vec<_> = {
            let map = lock(&self.grandchildren)?;
            map.values()
                .flatten()
                .map(|gc| (gc.pid, gc.task.clone()))
                .collect()
        };
        // Signal every grandchild first and wait for all of their tasks
        // concurrently, so that the shutdown takes as long as the slowest
        // child instead of the sum of all of them.
        let mut waits = Vec::with_capacity(children.len());
        for (pid, task) in children {
            let span = debug_span!("kill_grandchild", pid);
            let _enter = span.enter();
            debug!("Killing single grandchild");
            kill_grandchild(pid, s);
            if let Some(t) = task
                && let Some(handle) = lock(&t)?.take()
            {
                let close_span = debug_span!("close", signal = s.as_str());
                waits.push(
                    async move {
                        debug!("Waiting for task to close");
                        if let Err(e) = handle.await {
                            error!("Unable to close grandchild: {:#}", e)
                        }
                        debug!("Done killing single grandchild");
                    }
                    .instrument(close_span),
                );
            }
        }
        join_all(waits).await;
        debug!("Done killing all grandchildren");
        Ok(())
    }
}

pub fn kill_grandchild(raw_pid: u32, s: Signal) {
    let pid = Pid::from_raw(raw_pid as pid_t);
    if let Ok(pgid) = getpgid(Some(pid)) {
        // If process_group is 1, we will end up calling
        // kill(-1), which kills everything conmon is allowed to.
        let pgid = i32::from(pgid);

        #[allow(clippy::collapsible_if)]
        if pgid > 1 {
            if let Err(e) = kill(Pid::from_raw(-pgid), s) {
                error!(
                    raw_pid,
                    "Failed to kill process group {pgid}, falling back to killing pid: {:#}", e
                );
            }
        }
    }
    if let Err(e) = kill(pid, s) {
        debug!("Failed killing pid: {:#}", e);
    }
}

type TaskHandle = Arc<Mutex<Option<JoinHandle<()>>>>;

#[derive(Clone, Debug)]
pub struct ReapableChild {
    kind: ChildKind,
    exit_paths: Arc<[PathBuf]>,
    oom_exit_paths: Arc<[PathBuf]>,
    pid: u32,
    io: SharedContainerIO,
    timeout: Option<Instant>,
    token: CancellationToken,
    task: Option<TaskHandle>,
    cleanup_cmd: Arc<[String]>,
}

impl ReapableChild {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn io(&self) -> &SharedContainerIO {
        &self.io
    }

    pub fn timeout(&self) -> Option<Instant> {
        self.timeout
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }
}

#[derive(Clone, Debug)]
pub struct ExitChannelData {
    pub exit_code: i32,
    pub timed_out: bool,
}

impl ReapableChild {
    pub fn from_child(child: Child) -> (Box<str>, Self) {
        (
            child.id,
            Self {
                kind: child.kind,
                exit_paths: Arc::from(child.exit_paths),
                oom_exit_paths: Arc::from(child.oom_exit_paths),
                pid: child.pid,
                io: child.io,
                timeout: child.timeout,
                token: child.token,
                task: None,
                cleanup_cmd: Arc::from(child.cleanup_cmd),
            },
        )
    }

    fn watch(&mut self) -> (Sender<ExitChannelData>, Receiver<ExitChannelData>) {
        let exit_paths = self.exit_paths.clone();
        let oom_exit_paths = self.oom_exit_paths.clone();
        let pid = self.pid();
        // Only one exit code will be written.
        let (exit_tx, exit_rx) = broadcast::channel(1);
        let exit_tx_clone = exit_tx.clone();
        let timeout = self.timeout();
        let stop_token = self.token().clone();
        let cleanup_cmd_raw = self.cleanup_cmd.clone();

        let task = task::spawn(
            async move {
                debug!("Running task");
                let mut exit_code: i32 = -1;
                let mut oomed = false;
                let mut timed_out = false;
                let (oom_tx, mut oom_rx) = tokio::sync::mpsc::channel(1);
                let oom_watcher = OOMWatcher::new(&stop_token, pid, &oom_exit_paths, oom_tx);

                let stop_token_for_timeout = stop_token.clone();
                let wait_for_exit_code = Self::wait_for_exit_code_async(stop_token, pid)
                    .instrument(debug_span!("wait_for_exit_code"));

                let closure = async {
                    let (code, oom) = tokio::join!(wait_for_exit_code, oom_rx.recv());
                    exit_code = code;
                    if let Some(event) = oom {
                        oomed = event.oom;
                    }
                };
                if let Some(timeout) = timeout {
                    if time::timeout_at(timeout, closure).await.is_err() {
                        timed_out = true;
                        exit_code = -3;
                        kill_grandchild(pid, Signal::SIGKILL);
                        stop_token_for_timeout.cancel();
                        // Reap the zombie: the pidfd future was dropped by the
                        // timeout so waitpid never ran. The thread survives
                        // cancellation and will reap once SIGKILL takes effect.
                        Self::spawn_waitpid_thread(move || {
                            let _ = waitpid(Pid::from_raw(pid as pid_t), None);
                        });
                    }
                } else {
                    closure.await;
                }
                oom_watcher.stop();

                let exit_channel_data = ExitChannelData {
                    exit_code,
                    timed_out,
                };
                debug!(?exit_paths, oomed, "Write to exit paths");
                Self::write_to_exit_paths(exit_code, &exit_paths).await;

                if !cleanup_cmd_raw.is_empty() {
                    Self::spawn_cleanup_process(&cleanup_cmd_raw).await;
                }

                debug!("Sending exit struct to channel: {:?}", exit_channel_data);
                if exit_tx_clone.send(exit_channel_data).is_err() {
                    debug!("Unable to send exit status");
                }
                debug!("Task done");
            }
            .instrument(debug_span!("watch", pid)),
        );

        self.task = Some(Arc::new(Mutex::new(Some(task))));

        (exit_tx, exit_rx)
    }

    async fn spawn_cleanup_process(raw_cmd: &[String]) {
        let mut cleanup_cmd = Command::new(&raw_cmd[0]);

        cleanup_cmd.args(&raw_cmd[1..]);

        tokio::spawn(async move {
            match cleanup_cmd.status().await {
                Ok(status) => {
                    if !status.success() {
                        error!("Failed to execute cleanup command successfully: {}", status);
                    }
                }
                Err(e) => error!(
                    "Failed to spawn and execute cleanup command process successfully: {}",
                    e
                ),
            }
        });
    }

    /// Wait for a process to exit using pidfd_open for async polling,
    /// with a blocking waitpid fallback for older kernels.
    pub(crate) async fn wait_for_exit_code_async(token: CancellationToken, pid: u32) -> i32 {
        const FAILED_EXIT_CODE: i32 = -3;
        debug!("Waiting for exit code of pid {pid}");

        let exit_code = match Self::try_pidfd_wait(pid).await {
            Some(code) => code,
            None => {
                debug!("pidfd not available, falling back to blocking waitpid");
                let (tx, rx) = oneshot::channel();
                Self::spawn_waitpid_thread(move || {
                    let _ = tx.send(Self::wait_for_exit_code_blocking(pid));
                });
                rx.await.unwrap_or_else(|_| {
                    error!("waitpid thread exited without a result");
                    FAILED_EXIT_CODE
                })
            }
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
            if let Some(f) = slot.lock().ok().and_then(|mut f| f.take()) {
                f()
            }
        }

        let thread_slot = slot.clone();
        if let Err(e) = thread::Builder::new()
            .name("waitpid".into())
            .stack_size(Self::WAITPID_STACK_SIZE)
            .spawn(move || run(&thread_slot))
        {
            error!(
                "Unable to spawn waitpid thread, using blocking pool: {:#}",
                e
            );
            task::spawn_blocking(move || run(&slot));
        }
    }

    /// Try to use pidfd_open + async polling. Returns None if pidfd is unavailable.
    async fn try_pidfd_wait(pid: u32) -> Option<i32> {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};

        // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor
        // or a negative error, it does not touch any memory of ours.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0) };
        if fd < 0 {
            return None;
        }
        // SAFETY: the descriptor was just returned by the kernel and is not
        // owned by anything else.
        let owned_fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };

        let flags = fcntl(&owned_fd, FcntlArg::F_GETFL).ok()?;
        let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        fcntl(&owned_fd, FcntlArg::F_SETFL(flags)).ok()?;

        // SAFETY: `owned_fd` is an open pidfd that the `AsyncFd` takes ownership
        // of, so it stays valid and unchanged for the lifetime of the `AsyncFd`.
        let async_fd = match unsafe { AsyncFd::register(owned_fd) } {
            Ok(fd) => fd,
            Err(e) => {
                error!("Failed to create AsyncFd from pidfd: {:#}", e);
                return None;
            }
        };

        if async_fd.readable().await.is_err() {
            return None;
        }

        match waitpid(Pid::from_raw(pid as pid_t), WaitPidFlag::WNOHANG.into()) {
            Ok(WaitStatus::Exited(_, code)) => {
                debug!("Exited {code}");
                Some(code)
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                debug!("Signaled: {sig}");
                Some((sig as i32) + 128)
            }
            Ok(_) => {
                debug!("Unexpected wait status after pidfd readable");
                Some(-1)
            }
            Err(e) => {
                error!("waitpid after pidfd readable failed: {:#}", e);
                Some(-1)
            }
        }
    }

    fn wait_for_exit_code_blocking(pid: u32) -> i32 {
        const FAILED_EXIT_CODE: i32 = -3;
        loop {
            match waitpid(Pid::from_raw(pid as pid_t), None) {
                Ok(WaitStatus::Exited(_, exit_code)) => {
                    debug!("Exited {}", exit_code);
                    return exit_code;
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    debug!("Signaled: {sig}");
                    return (sig as i32) + 128;
                }
                Ok(_) => {
                    continue;
                }
                Err(Errno::EINTR) => {
                    debug!("Failed to wait for pid on EINTR, retrying");
                    continue;
                }
                Err(err) => {
                    error!("Unable to waitpid on {:#}", err);
                    return FAILED_EXIT_CODE;
                }
            };
        }
    }

    /// Write the exit code to every exit path. Failures are logged per path,
    /// one unwritable path must not prevent the others.
    async fn write_to_exit_paths(code: i32, paths: &[PathBuf]) {
        let code_str = format!("{code}");
        for path_buf in paths {
            let span = debug_span!("write_exit_path", path = %path_buf.display());
            async {
                debug!(code, "Writing exit code to file");
                if let Err(e) = fs::write(path_buf, &code_str).await {
                    error!("Could not write exit file: {:#}", e);
                }
            }
            .instrument(span)
            .await;
        }
    }
}
