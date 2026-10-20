//! Child process reaping and management.
use crate::fd_mapping::{CommandFdExt, FdMapping};
use crate::{
    child::{Child, ChildKind},
    container_io::{ContainerIO, ContainerIOType, SharedContainerIO},
    mutex::lock,
    oom_watcher::OOMWatcher,
};
use anyhow::{Context, Result, format_err};
use futures::future::join_all;
use libc::pid_t;
use nix::{
    errno::Errno,
    sys::{
        signal::{Signal, kill},
        wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid},
    },
    unistd::{Pid, getpgid, getpid},
};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fmt::Debug,
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
    process::{Child as RuntimeChild, Command},
    select,
    signal::unix::{SignalKind, signal},
    sync::{
        Notify,
        broadcast::{self, Receiver},
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

    /// The runtimes and container processes of calls in progress, which are
    /// not registered as grandchildren yet. Their exit status belongs to the
    /// call, so the orphan sweep leaves them alone.
    pending: Arc<Mutex<HashSet<u32>>>,

    /// Wakes the orphan sweep up early, for example after a failed runtime
    /// call or on shutdown.
    sweep: Arc<Notify>,
}

/// Marks a PID as pending for the lifetime of the guard.
struct PendingPid {
    pending: Arc<Mutex<HashSet<u32>>>,
    pid: Option<u32>,
}

impl PendingPid {
    fn new(pending: &Arc<Mutex<HashSet<u32>>>, pid: Option<u32>) -> Self {
        if let Some(pid) = pid
            && let Ok(mut set) = pending.lock()
        {
            set.insert(pid);
        }
        Self {
            pending: pending.clone(),
            pid,
        }
    }
}

impl Drop for PendingPid {
    fn drop(&mut self) {
        if let Some(pid) = self.pid
            && let Ok(mut set) = self.pending.lock()
        {
            set.remove(&pid);
        }
    }
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

        let _runtime_pending = PendingPid::new(&self.pending, child.id());

        debug!(
            "Running child on PID: {}",
            child.id().map_or("unknown".into(), |x| x.to_string())
        );

        let token = CancellationToken::new();

        let connected = match container_io.typ_mut() {
            ContainerIOType::Terminal(terminal) => {
                // The runtime connects to the console socket before it exits.
                // If it fails before that, waiting for the connection alone
                // would hang this call forever.
                select! {
                    res = terminal.wait_connected(stdin, token.clone()) => {
                        res.context("wait for terminal socket connection")
                    }
                    status = child.wait() => match status.context("wait for child") {
                        // The runtime succeeded, so it did connect and the
                        // descriptor is queued already.
                        Ok(status) if status.success() => time::timeout(
                            Self::CONNECT_AFTER_EXIT_TIMEOUT,
                            terminal.wait_connected(stdin, token.clone()),
                        )
                        .await
                        .context("runtime exited without connecting the terminal")
                        .and_then(|res| res.context("wait for terminal socket connection")),
                        // A failure is reported below together with its output.
                        Ok(_) => Ok(()),
                        Err(e) => Err(e),
                    },
                }
            }
            ContainerIOType::Streams(streams) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let stdin = child.stdin.take();
                streams.handle_stdio_receive(stdin, stdout, stderr, token.clone());
                Ok(())
            }
        };

        let output = match connected {
            Ok(()) => child.wait_with_output().await.context("wait for runtime")?,
            Err(e) => {
                token.cancel();
                let stderr = Self::stop_runtime(child).await;
                return Err(self.fail_create(pidfile, e, &stderr).await);
            }
        };

        if !output.status.success() {
            // token must be cancelled here because the child hasn't been registered yet,
            // meaning there is no other entity that could cancel the read_loops.
            token.cancel();
            let error = match output.status.code() {
                Some(code) => format_err!("child command exited with: {code}"),
                None => format_err!("child command exited with signal"),
            };
            return Err(self.fail_create(pidfile, error, &output.stderr).await);
        }

        // Read synchronously and mark the process as pending right away, a
        // container process which exits before it is registered must not be
        // taken for an orphan.
        let grandchild_pid = std::fs::read_to_string(pidfile)
            .with_context(|| format!("grandchild pid read error {}", pidfile.display()))?
            .trim()
            .parse::<u32>()
            .with_context(|| format!("grandchild pid parse error {}", pidfile.display()))?;

        // Registered by the caller through `watch_grandchild`, which takes it
        // out of the pending set again.
        lock(&self.pending)?.insert(grandchild_pid);

        Ok((grandchild_pid, token))
    }

    /// Stop a runtime which failed before it exited: give it a bounded time
    /// to exit on its own, so that it still writes the PID file the cleanup
    /// needs, kill it otherwise, and return its stderr.
    async fn stop_runtime(mut child: RuntimeChild) -> Vec<u8> {
        if time::timeout(Self::CONNECT_AFTER_EXIT_TIMEOUT, child.wait())
            .await
            .is_err()
        {
            debug!("Runtime did not exit after a failed console connection, killing it");
            if let Err(e) = child.start_kill() {
                debug!("Unable to kill runtime: {e}");
            }
        }

        // Its stderr usually holds the reason. A helper of the runtime may
        // keep the pipes open though, so do not wait for them forever.
        match time::timeout(Self::CONNECT_AFTER_EXIT_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => output.stderr,
            Ok(Err(e)) => {
                debug!("Unable to wait for runtime: {e}");
                vec![]
            }
            Err(_) => {
                debug!("Runtime output still held open, giving up on it");
                vec![]
            }
        }
    }

    /// Clean up after a failed runtime call and build its error, with the
    /// stderr of the runtime appended.
    async fn fail_create(
        &self,
        pidfile: &Path,
        error: anyhow::Error,
        stderr: &[u8],
    ) -> anyhow::Error {
        // Do not leave a container process behind which the runtime created
        // before it failed, and reap what else it left.
        Self::cleanup_failed_grandchild(pidfile).await;
        self.sweep.notify_one();

        let stderr = String::from_utf8_lossy(stderr);
        let stderr = stderr.trim();
        let error = if stderr.is_empty() {
            error
        } else {
            format_err!("{error:#}: {stderr}")
        };
        error!("Failed: {error:#}");
        error
    }

    /// How long a zombie nobody claimed stays unreaped before the sweep takes
    /// it, so that an owner which is about to wait for it still can.
    const ORPHAN_GRACE: Duration = Duration::from_secs(1);

    /// Reap exited children which nobody waits for, in the background.
    ///
    /// A runtime which fails after entering the PID namespace of a container,
    /// like an exec of a missing binary, leaves its helpers behind as children
    /// of this subreaper. They exit at any later time. The init of the
    /// namespace cannot exit until their zombies are reaped, so the container
    /// could never be stopped.
    ///
    /// Every other child has an owner which reaps it: the runtimes are waited
    /// for by tokio, the container processes by their watchers, and the
    /// cleanup commands and the pause init by the tasks which spawned them.
    /// They are never zombies for long, so a zombie which is neither
    /// registered nor pending for [`ChildReaper::ORPHAN_GRACE`] is an orphan.
    pub fn start_orphan_sweep(self: Arc<Self>) {
        task::spawn(
            async move {
                let mut sigchld = match signal(SignalKind::child()) {
                    Ok(signal) => signal,
                    Err(e) => {
                        error!("Unable to listen for SIGCHLD, orphans stay unreaped: {e}");
                        return;
                    }
                };
                let mut candidates = HashMap::new();
                loop {
                    select! {
                        _ = sigchld.recv() => {}
                        _ = self.sweep.notified() => {}
                        // Only candidates need a recheck once their grace is over.
                        _ = time::sleep(Self::ORPHAN_GRACE), if !candidates.is_empty() => {}
                    }
                    self.sweep_orphans(&mut candidates, Self::ORPHAN_GRACE)
                        .await;
                }
            }
            .instrument(debug_span!("orphan_sweep")),
        );
    }

    /// Reap the zombie children which have been unclaimed for `grace`.
    /// `candidates` remembers when a zombie was first seen.
    async fn sweep_orphans(&self, candidates: &mut HashMap<u32, Instant>, grace: Duration) {
        let zombies = match task::spawn_blocking(Self::zombie_children).await {
            Ok(zombies) => zombies,
            Err(e) => {
                error!("Unable to scan for zombie children: {e}");
                return;
            }
        };

        let now = Instant::now();
        candidates.retain(|pid, _| zombies.contains(pid));
        for pid in zombies {
            let first_seen = *candidates.entry(pid).or_insert(now);
            if now.duration_since(first_seen) < grace {
                continue;
            }
            // Check the owners right before reaping, the sets change while
            // the scan runs.
            if self.is_claimed(pid) {
                candidates.remove(&pid);
                continue;
            }
            match waitpid(Pid::from_raw(pid as pid_t), Some(WaitPidFlag::WNOHANG)) {
                Ok(status) => debug!("Reaped orphaned child {pid}: {status:?}"),
                Err(e) => debug!("Unable to reap orphaned child {pid}: {e}"),
            }
            candidates.remove(&pid);
        }
    }

    /// Whether a watcher or a call in progress waits for the child.
    fn is_claimed(&self, pid: u32) -> bool {
        let registered = self
            .grandchildren
            .lock()
            .is_ok_and(|map| map.values().flatten().any(|child| child.pid == pid));
        registered
            || self
                .pending
                .lock()
                .is_ok_and(|pending| pending.contains(&pid))
    }

    /// The zombie children of this process.
    fn zombie_children() -> Vec<u32> {
        Self::children()
            .into_iter()
            .filter(|pid| Self::is_zombie(*pid))
            .collect()
    }

    /// The children of all threads of this process. The kernel lists them in
    /// `/proc/self/task/<tid>/children`, which needs `CONFIG_PROC_CHILDREN`,
    /// fall back to scanning all processes without it.
    fn children() -> Vec<u32> {
        let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
            return vec![];
        };
        let mut children = vec![];
        for task in tasks.flatten() {
            let Ok(list) = std::fs::read_to_string(task.path().join("children")) else {
                return Self::children_by_scan();
            };
            children.extend(
                list.split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok()),
            );
        }
        children
    }

    fn children_by_scan() -> Vec<u32> {
        let parent = format!("PPid:\t{}", getpid());
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return vec![];
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/status"))
                    .is_ok_and(|status| status.lines().any(|line| line == parent))
            })
            .collect()
    }

    /// Whether the process is a zombie. The state follows the parenthesized
    /// command name in `/proc/<pid>/stat`.
    fn is_zombie(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z'))
        })
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
        lock(&self.pending)?.remove(&pid);

        // Create channels outside the lock to minimize contention
        let (exit_rx, mut forget_rx) = reapable_grandchild.watch();

        let locked_grandchildren = self.grandchildren();
        lock(locked_grandchildren)?
            .entry(id)
            .or_default()
            .push(reapable_grandchild);
        let cleanup_grandchildren = locked_grandchildren.clone();

        task::spawn(
            async move {
                forget_rx.recv().await?;
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
        // An init of a PID namespace only exits once every process of the
        // namespace is reaped, including zombies reparented to this process.
        // The sweep keeps running while the watchers are awaited.
        self.sweep.notify_one();
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

    /// Watch the process and return two receivers for its exit: one for the
    /// caller and one to forget the child afterwards. Both exist before the
    /// task starts, a receiver subscribed after the exit would miss it.
    fn watch(&mut self) -> (Receiver<ExitChannelData>, Receiver<ExitChannelData>) {
        let exit_paths = self.exit_paths.clone();
        let oom_exit_paths = self.oom_exit_paths.clone();
        let pid = self.pid();
        // Only one exit code will be written.
        let (exit_tx, exit_rx) = broadcast::channel(1);
        let forget_rx = exit_tx.subscribe();
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
                if exit_tx.send(exit_channel_data).is_err() {
                    debug!("Unable to send exit status");
                }
                debug!("Task done");
            }
            .instrument(debug_span!("watch", pid)),
        );

        self.task = Some(Arc::new(Mutex::new(Some(task))));

        (exit_rx, forget_rx)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Spawn a child which exits right away and is never waited for.
    fn spawn_zombie() -> Result<u32> {
        let child = Command::new("true").spawn()?;
        let pid = child.id();
        std::mem::forget(child);
        let start = Instant::now();
        while !ChildReaper::is_zombie(pid) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "child never became a zombie"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(pid)
    }

    /// One test for all of it, the sweep reaps every unclaimed zombie of the
    /// test process, so zombies of a parallel test would interfere.
    #[tokio::test]
    async fn sweep_reaps_unclaimed_zombies_after_grace() -> Result<()> {
        let reaper = ChildReaper::default();
        let orphan = spawn_zombie()?;
        let claimed = spawn_zombie()?;
        lock(&reaper.pending)?.insert(claimed);

        // Both ways of listing the children find them.
        assert!(ChildReaper::zombie_children().contains(&orphan));
        assert!(ChildReaper::children_by_scan().contains(&orphan));

        // The first sweep only records the candidates.
        let mut candidates = HashMap::new();
        reaper
            .sweep_orphans(&mut candidates, Duration::from_secs(60))
            .await;
        assert!(ChildReaper::is_zombie(orphan));

        // Without grace the unclaimed one is reaped, the claimed one stays.
        reaper.sweep_orphans(&mut candidates, Duration::ZERO).await;
        assert!(!ChildReaper::is_zombie(orphan));
        assert!(ChildReaper::is_zombie(claimed));
        assert!(!candidates.contains_key(&orphan));

        waitpid(Pid::from_raw(claimed as pid_t), None)?;
        assert!(!ChildReaper::zombie_children().contains(&claimed));
        Ok(())
    }
}
