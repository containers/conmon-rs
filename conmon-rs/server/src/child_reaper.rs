//! Child process reaping and management.
use crate::fd_mapping::{CommandFdExt, FdMapping};
use crate::{
    attach::SharedContainerAttach,
    child::{Child, ChildKind, sleep_until},
    container_io::{ContainerIO, ContainerIOType, Drain, SharedContainerIO},
    init,
    mutex::lock,
    oom_watcher::{OOMEvent, OOMWatcher},
    process::{Process, is_zombie, pidfd_open, reap_guard},
};
use anyhow::{Context, Result, bail, format_err};
use futures::future::join_all;
use libc::pid_t;
use nix::{
    errno::Errno,
    sys::{
        signal::Signal,
        wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid},
    },
    unistd::{Pid, getpid},
};
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fmt::Debug,
    os::fd::{OwnedFd, RawFd},
    path::{Path, PathBuf},
    process::Stdio,
    str,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
use tokio::{
    fs,
    io::unix::AsyncFd,
    io::{AsyncRead, AsyncReadExt},
    process::{Child as RuntimeChild, ChildStderr, ChildStdout, Command},
    select,
    signal::unix::{SignalKind, signal},
    sync::{
        Notify,
        broadcast::{self, Receiver},
        oneshot, watch,
    },
    task::{self, JoinHandle},
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, warn};

/// The stdout and stderr of a runtime with pipes of its own, read in the
/// background until they end. Only the end of stderr is kept, it holds the
/// error, stdout is not of interest.
struct RuntimeOutput {
    stderr: Arc<Mutex<Vec<u8>>>,
    task: JoinHandle<()>,
}

impl RuntimeOutput {
    /// How much of the end of stderr is kept.
    const MAX_STDERR: usize = 64 * 1024;

    fn spawn(stdout: Option<ChildStdout>, stderr: Option<ChildStderr>) -> Self {
        let buf = Arc::<Mutex<Vec<u8>>>::default();
        let stderr_buf = buf.clone();
        let task = task::spawn(async move {
            tokio::join!(
                async {
                    if let Some(stdout) = stdout {
                        Self::read(stdout, None).await
                    }
                },
                async {
                    if let Some(stderr) = stderr {
                        Self::read(stderr, Some(&stderr_buf)).await
                    }
                },
            );
        });
        Self { stderr: buf, task }
    }

    /// Read until the end, keeping the end in `buf` if any.
    async fn read(mut reader: impl AsyncRead + Unpin, buf: Option<&Mutex<Vec<u8>>>) {
        let mut chunk = [0; 8192];
        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let Some(buf) = buf else {
                continue;
            };
            let Ok(mut buf) = buf.lock() else {
                return;
            };
            buf.extend_from_slice(&chunk[..n]);
            // Dropping the front only once it doubled keeps it amortized.
            if buf.len() > 2 * Self::MAX_STDERR {
                let excess = buf.len() - Self::MAX_STDERR;
                buf.drain(..excess);
            }
        }
    }

    /// Wait for the end of the output for a while, a helper of the runtime
    /// may hold the pipes open, and return the end of stderr.
    async fn finish(output: Option<Self>) -> Vec<u8> {
        let Some(mut output) = output else {
            return vec![];
        };
        if time::timeout(ChildReaper::CONNECT_AFTER_EXIT_TIMEOUT, &mut output.task)
            .await
            .is_err()
        {
            debug!("Runtime output still held open, giving up on it");
            output.task.abort();
        }
        let mut stderr = match output.stderr.lock() {
            Ok(mut stderr) => std::mem::take(&mut *stderr),
            Err(_) => return vec![],
        };
        let excess = stderr.len().saturating_sub(Self::MAX_STDERR);
        stderr.drain(..excess);
        stderr
    }
}

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

    /// Set by the shutdown while it holds the lock of the grandchildren, so a
    /// child registered later knows that the shutdown missed it.
    shutting_down: AtomicBool,

    /// The runtime calls in progress, including the registration of their
    /// child, which the shutdown waits for.
    in_flight: Arc<watch::Sender<usize>>,

    /// The watchers of children registered after the shutdown took its
    /// snapshot, which it waits for as well.
    late_watchers: Mutex<Vec<TaskHandle>>,
}

/// Counts a runtime call as in progress until dropped. A successful
/// [`ChildReaper::create_child`] hands it to the caller, which passes it to
/// [`ChildReaper::watch_grandchild`], so the call ends with the registration
/// of its child, or wherever the caller gives up.
#[derive(Debug)]
pub struct InFlight(Option<Arc<watch::Sender<usize>>>);

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(in_flight) = &self.0 {
            in_flight.send_modify(|n| *n = n.saturating_sub(1));
        }
    }
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

/// first usable file descriptor after stdin, stdout and stderr
const FIRST_FD_AFTER_STDIO: RawFd = 3;

impl ChildReaper {
    /// How long to wait for the console connection of a runtime which already
    /// exited successfully.
    const CONNECT_AFTER_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

    pub fn grandchildren(&self) -> &Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>> {
        &self.grandchildren
    }

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
        runtime_deadline: Option<Instant>,
        env_vars: Vec<(String, String)>,
        relays_notify: bool,
        additional_fds: Vec<OwnedFd>,
    ) -> Result<(u32, CancellationToken, InFlight)>
    where
        P: AsRef<OsStr> + Debug,
        I: IntoIterator<Item = S> + Debug,
        S: AsRef<OsStr>,
    {
        let in_flight = self.begin_call()?;
        debug!("Running: {:?} {:?}", cmd, args);
        let mut cmd = Command::new(cmd);
        // A runtime which gets dropped, for example on a panic, must not keep
        // running unobserved.
        cmd.kill_on_drop(true);
        init::reset_oom_score(&mut cmd);

        if stdin {
            cmd.stdin(Stdio::piped());
        }

        // With the server relaying the sd_notify messages, the runtime must
        // not add its own notify socket mount and `NOTIFY_SOCKET` to the
        // container on top of what the caller set up. A value passed in the
        // environment variables on purpose still reaches it.
        if relays_notify {
            cmd.env_remove("NOTIFY_SOCKET");
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

        // With streams, the read loops take the output of the runtime along
        // with the one of the process. With a terminal, the runtime has pipes
        // of its own. They get read while waiting for it, a runtime which
        // writes more than they hold, like with debug logging, would block.
        let mut output = None;

        // How long a runtime which failed before it exited may take to exit on
        // its own, none once its deadline passed.
        let mut grace = Self::CONNECT_AFTER_EXIT_TIMEOUT;
        let connected = match container_io.typ_mut() {
            ContainerIOType::Terminal(terminal) => {
                output = Some(RuntimeOutput::spawn(
                    child.stdout.take(),
                    child.stderr.take(),
                ));
                // The runtime connects to the console socket before it exits.
                // If it fails before that, waiting for the connection alone
                // would hang this call forever.
                select! {
                    res = terminal.wait_connected(stdin, token.clone()) => {
                        res.context("wait for terminal socket connection")
                    }
                    () = sleep_until(runtime_deadline) => {
                        grace = Duration::ZERO;
                        Err(format_err!("runtime did not finish in time"))
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

        let connected = match (connected, runtime_deadline) {
            // A runtime which hangs gets killed once its deadline passed.
            (Ok(()), Some(deadline)) => match time::timeout_at(deadline, child.wait()).await {
                Ok(_) => Ok(()),
                Err(_) => {
                    grace = Duration::ZERO;
                    Err(format_err!("runtime did not finish in time"))
                }
            },
            (res, _) => res,
        };
        let status = match connected {
            Ok(()) => child.wait().await.context("wait for runtime")?,
            Err(e) => {
                token.cancel();
                let stderr = Self::stop_runtime(child, output, grace).await;
                return Err(self.fail_create(pidfile, e, &stderr).await);
            }
        };
        let stderr = RuntimeOutput::finish(output).await;

        if !status.success() {
            // token must be cancelled here because the child hasn't been registered yet,
            // meaning there is no other entity that could cancel the read_loops.
            token.cancel();
            let error = match status.code() {
                Some(code) => format_err!("child command exited with: {code}"),
                None => format_err!("child command exited with signal"),
            };
            return Err(self.fail_create(pidfile, error, &stderr).await);
        }

        // Read synchronously and mark the process as pending right away, a
        // container process which exits before it is registered must not be
        // taken for an orphan.
        let grandchild_pid = match Self::read_pidfile(pidfile) {
            Ok(pid) => pid,
            Err(e) => {
                // Nothing registers the process, so the read loops have to be
                // stopped here as well.
                token.cancel();
                return Err(self.fail_create(pidfile, e, &stderr).await);
            }
        };

        // Registered by the caller through `watch_grandchild`, which takes it
        // out of the pending set again and ends the call for the shutdown.
        lock(&self.pending)?.insert(grandchild_pid);

        Ok((grandchild_pid, token, in_flight))
    }

    fn read_pidfile(pidfile: &Path) -> Result<u32> {
        std::fs::read_to_string(pidfile)
            .with_context(|| format!("grandchild pid read error {}", pidfile.display()))?
            .trim()
            .parse::<u32>()
            .with_context(|| format!("grandchild pid parse error {}", pidfile.display()))
    }

    /// Start a runtime call, unless the server shuts down. The shutdown
    /// waits for the calls in progress, see [`ChildReaper::kill_grandchildren`].
    fn begin_call(&self) -> Result<InFlight> {
        // Under the lock the shutdown takes its snapshot with, so that it
        // either sees this call or this call sees the shutdown.
        let _map = lock(&self.grandchildren)?;
        if self.shutting_down.load(Ordering::SeqCst) {
            bail!("server is shutting down")
        }
        self.in_flight.send_modify(|n| *n += 1);
        Ok(InFlight(Some(self.in_flight.clone())))
    }

    /// Stop a runtime which failed before it exited: give it `grace` to exit
    /// on its own, so that it still writes the PID file the cleanup needs,
    /// kill it otherwise, and return its stderr.
    async fn stop_runtime(
        mut child: RuntimeChild,
        output: Option<RuntimeOutput>,
        grace: Duration,
    ) -> Vec<u8> {
        if time::timeout(grace, child.wait()).await.is_err() {
            debug!("Runtime did not exit in time, killing it");
            if let Err(e) = child.start_kill() {
                debug!("Unable to kill runtime: {e}");
            }
            // Its container process gets reparented to this server only once
            // it is gone, the cleanup relies on that. A runtime stuck in the
            // kernel is not waited for forever.
            match time::timeout(Self::CONNECT_AFTER_EXIT_TIMEOUT, child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => debug!("Unable to wait for runtime: {e}"),
                Err(_) => warn!("Killed runtime did not exit, cleaning up anyway"),
            }
        }

        // Its stderr usually holds the reason.
        RuntimeOutput::finish(output).await
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
        self.cleanup_failed_grandchild(pidfile).await;
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
        let (zombies, complete) = match task::spawn_blocking(Self::zombie_children).await {
            Ok(zombies) => zombies,
            Err(e) => {
                error!("Unable to scan for zombie children: {e}");
                return;
            }
        };
        // The children of a thread which exited meanwhile got another thread
        // as parent, which may have been listed already. The next sweep finds
        // them.
        if !complete {
            self.sweep.notify_one();
        }

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
            let reaping = reap_guard();
            let status = waitpid(Pid::from_raw(pid as pid_t), Some(WaitPidFlag::WNOHANG));
            drop(reaping);
            match status {
                Ok(status) => debug!("Reaped orphaned child {pid}: {status:?}"),
                Err(e) => debug!("Unable to reap orphaned child {pid}: {e}"),
            }
            candidates.remove(&pid);
        }
    }

    /// Whether a watcher or a call in progress waits for the child.
    ///
    /// Both sets get checked under the lock of the grandchildren, which
    /// `watch_grandchild` holds while it moves a child from one to the other.
    fn is_claimed(&self, pid: u32) -> bool {
        let Ok(map) = self.grandchildren.lock() else {
            return false;
        };
        map.values().flatten().any(|child| child.pid == pid)
            || self
                .pending
                .lock()
                .is_ok_and(|pending| pending.contains(&pid))
    }

    /// The zombie children of this process, and whether the list is
    /// complete, see [`Self::children`].
    fn zombie_children() -> (Vec<u32>, bool) {
        let (children, complete) = Self::children();
        let zombies = children.into_iter().filter(|pid| is_zombie(*pid)).collect();
        (zombies, complete)
    }

    /// The children of all threads of this process. The kernel lists them in
    /// `/proc/self/task/<tid>/children`, which needs `CONFIG_PROC_CHILDREN`,
    /// fall back to scanning all processes without it. The list is not
    /// complete if a thread exited while listing them.
    fn children() -> (Vec<u32>, bool) {
        static PROC_CHILDREN: LazyLock<bool> =
            LazyLock::new(|| Path::new("/proc/thread-self/children").exists());
        if !*PROC_CHILDREN {
            return (Self::children_by_scan(), true);
        }
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
                    .filter_map(|pid| pid.parse::<u32>().ok()),
            );
        }
        (children, complete)
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

    /// Kill and reap the container process of a failed runtime call, if the
    /// runtime got as far as writing its PID. It got reparented to this
    /// subreaper and nobody else waits for it.
    ///
    /// Only this PID is waited for. Reaping any exited child instead would take
    /// the exit status of runtimes of concurrent calls, or of grandchildren
    /// which are not registered yet.
    async fn cleanup_failed_grandchild(&self, pidfile: &Path) {
        let Ok(content) = fs::read_to_string(pidfile).await else {
            return;
        };
        let Ok(pid) = content.trim().parse::<pid_t>() else {
            return;
        };
        // A stale PID file may name a process which got reused by a container
        // or exec of this server, which must not be killed.
        if self.is_claimed(pid as u32) {
            debug!("Not cleaning up PID {pid} of failed runtime call, it is in use");
            return;
        }
        let pid = Pid::from_raw(pid);

        // Only a child, the PID may have been reused otherwise.
        if let Err(e) = Process::child(pid).signal(Signal::SIGKILL) {
            debug!("Not cleaning up PID {pid} of failed runtime call: {e}");
            return;
        }
        debug!("Cleaning up PID {pid} of failed runtime call");
        task::spawn(ReapableChild::wait_for_exit_code_async(
            CancellationToken::new(),
            pid.as_raw() as u32,
        ));
    }

    /// Register and watch a child which `create_child` started, which ends
    /// its call.
    pub fn watch_grandchild(
        &self,
        child: Child,
        leak_fds: Vec<OwnedFd>,
        call: InFlight,
    ) -> Result<Receiver<ExitChannelData>> {
        let pid = child.pid;
        let (id, mut reapable_grandchild) = ReapableChild::from_child(child);

        let locked_grandchildren = self.grandchildren();
        let mut map = lock(locked_grandchildren)?;
        let (exit_rx, mut forget_rx) = reapable_grandchild.watch();
        let shutting_down = self.shutting_down.load(Ordering::SeqCst);
        if shutting_down && let Some(task) = &reapable_grandchild.task {
            lock(&self.late_watchers)?.push(task.clone());
        }
        map.entry(id).or_default().push(reapable_grandchild);
        // Only once registered, so it is claimed all the time, see
        // `is_claimed`.
        lock(&self.pending)?.remove(&pid);
        drop(call);
        drop(map);
        let cleanup_grandchildren = locked_grandchildren.clone();

        task::spawn(
            async move {
                // The watcher sends no exit if it went away, for example with a
                // panic. The child has to be forgotten either way.
                if let Err(e) = forget_rx.recv().await {
                    debug!("No exit status for {pid}: {e}");
                }
                drop(leak_fds);
                Self::forget_grandchild(&cleanup_grandchildren, pid)
            }
            .instrument(debug_span!("watch_grandchild", pid)),
        );

        // A call which was in progress while the shutdown killed the others.
        if shutting_down {
            kill_grandchild(pid, Signal::SIGKILL);
            bail!("server is shutting down")
        }
        Ok(exit_rx)
    }

    fn forget_grandchild(
        locked_grandchildren: &Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>>,
        grandchild_pid: u32,
    ) -> Result<()> {
        let mut map = lock(locked_grandchildren)?;
        let mut emptied = None;
        for (id, children) in map.iter_mut() {
            if let Some(pos) = children.iter().position(|v| v.pid == grandchild_pid) {
                children.swap_remove(pos);
                // Containers get a new ID on every restart, keeping the
                // entries of the old ones would grow the map forever.
                if children.is_empty() {
                    emptied = Some(id.clone());
                }
                break;
            }
        }
        if let Some(id) = emptied {
            map.remove(&id);
        }
        Ok(())
    }

    pub async fn kill_grandchildren(&self, s: Signal) -> Result<()> {
        debug!("Killing grandchildren");
        // Collect only PIDs and task handles to avoid cloning entire ReapableChild
        let children: Vec<_> = {
            let map = lock(&self.grandchildren)?;
            // Children registered after this snapshot get killed on their
            // registration, see `watch_grandchild`.
            self.shutting_down.store(true, Ordering::SeqCst);
            map.values()
                .flatten()
                .map(|gc| (gc.pid, gc.task.clone()))
                .collect()
        };
        // Signal every grandchild first and wait for all of their tasks
        // concurrently, so that the shutdown takes as long as the slowest
        // child instead of the sum of all of them.
        let mut tasks = Vec::with_capacity(children.len());
        for (pid, task) in children {
            let span = debug_span!("kill_grandchild", pid);
            let _enter = span.enter();
            debug!("Killing single grandchild");
            kill_grandchild(pid, s);
            tasks.extend(task);
        }
        // An init of a PID namespace only exits once every process of the
        // namespace is reaped, including zombies reparented to this process.
        // The sweep keeps running while the watchers are awaited.
        self.sweep.notify_one();
        Self::wait_for_watchers(tasks, s).await?;

        // Runtime calls in progress get to finish, the server would kill a
        // runtime halfway otherwise. Their children got killed on registration.
        let mut in_flight = self.in_flight.subscribe();
        if time::timeout(Self::IN_FLIGHT_WAIT, in_flight.wait_for(|n| *n == 0))
            .await
            .is_err()
        {
            warn!("Runtime calls still in progress, stopping anyway");
        }
        let late = std::mem::take(&mut *lock(&self.late_watchers)?);
        Self::wait_for_watchers(late, s).await?;
        debug!("Done killing all grandchildren");
        Ok(())
    }

    /// How long the shutdown waits for runtime calls in progress, below the
    /// time the clients give the whole shutdown.
    const IN_FLIGHT_WAIT: Duration = Duration::from_secs(5);

    async fn wait_for_watchers(tasks: Vec<TaskHandle>, s: Signal) -> Result<()> {
        let mut waits = Vec::with_capacity(tasks.len());
        for task in tasks {
            if let Some(handle) = lock(&task)?.take() {
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
        Ok(())
    }
}

/// Signal a grandchild and its process group, unless it got reaped already,
/// see [`Process::signal_group`]. The grandchildren got reparented to this
/// subreaper, so the PID refers to a child.
pub fn kill_grandchild(raw_pid: u32, s: Signal) {
    if let Err(e) = Process::child(Pid::from_raw(raw_pid as pid_t)).signal_group(s) {
        debug!(raw_pid, "Not signalling grandchild, it is gone: {e}");
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
    /// How long the result of the OOM watcher may take once the timeout of
    /// a process which exited in time passed. That covers confirming an
    /// event of a cgroup v1, see `OOMWatcher::CONFIRM_BACKOFF`.
    const OOM_RESULT_WAIT: Duration = Duration::from_secs(2);

    /// Wait a bounded time for the result of the OOM watcher, see
    /// [`ReapableChild::handle_oom`].
    async fn late_oom(
        oom_rx: &mut tokio::sync::mpsc::Receiver<OOMEvent>,
        exit_code: i32,
        oom_exit_paths: &[PathBuf],
    ) -> bool {
        let event = time::timeout(Self::OOM_RESULT_WAIT, oom_rx.recv())
            .await
            .ok()
            .flatten();
        if event.is_none() {
            debug!("No OOM result in time");
        }
        Self::handle_oom(event, exit_code, oom_exit_paths).await
    }

    /// Whether the process which exited with `exit_code` got OOM killed. The
    /// watcher writes the files for the OOMs it could tell on its own, this
    /// the ones which only the exit status tells.
    async fn handle_oom(
        event: Option<OOMEvent>,
        exit_code: i32,
        oom_exit_paths: &[PathBuf],
    ) -> bool {
        let Some(event) = event else {
            return false;
        };
        let oomed = event.is_oom(exit_code);
        if oomed && !event.oom {
            debug!("Taking the kill of the process for an OOM, its cgroup is gone");
            OOMWatcher::write_oom_files(oom_exit_paths).await;
        }
        oomed
    }

    /// Wait until the output ended, or until the [`Drain`] gives up. A
    /// process which inherited the pipes and keeps them open must not delay
    /// the exit file.
    async fn wait_for_output(attach: SharedContainerAttach) {
        let (mut rx, ended) = attach.subscribe();
        if ended {
            return;
        }
        let mut drain = Drain::default();
        while drain.recv_output(&mut rx, false).await.is_some() {}
    }

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
        let exit_token = stop_token.clone();
        let cleanup_cmd_raw = self.cleanup_cmd.clone();
        let io = self.io.clone();

        let task = task::spawn(
            async move {
                debug!("Running task");
                let mut exit_code: i32 = -1;
                let mut oomed = false;
                let mut timed_out = false;
                // Nobody learns about an OOM without the files, like an exec.
                // The sender gets dropped then, which ends the receiver.
                let (oom_tx, mut oom_rx) = tokio::sync::mpsc::channel(1);
                let oom_watcher = (!oom_exit_paths.is_empty())
                    .then(|| OOMWatcher::new(&stop_token, pid, &oom_exit_paths, oom_tx));

                let wait_for_exit_code = Self::wait_for_exit_code_async(stop_token, pid)
                    .instrument(debug_span!("wait_for_exit_code"));

                // Known once the process got reaped, also if the timeout hits
                // while waiting for the OOM watcher.
                let mut reaped = None;
                let closure = async {
                    let wait_for_exit_code = async {
                        let code = wait_for_exit_code.await;
                        reaped = Some(code);
                        code
                    };
                    let (code, oom) = tokio::join!(wait_for_exit_code, oom_rx.recv());
                    exit_code = code;
                    oomed = Self::handle_oom(oom, code, &oom_exit_paths).await;
                };
                if let Some(timeout) = timeout {
                    let res = time::timeout_at(timeout, closure).await;
                    if let (Err(_), Some(code)) = (&res, reaped) {
                        // It exited in time, only the OOM check did not end.
                        debug!("Process exited before its timeout");
                        exit_code = code;
                        oomed = Self::late_oom(&mut oom_rx, code, &oom_exit_paths).await;
                    } else if res.is_err() {
                        timed_out = true;
                        exit_code = -3;
                        kill_grandchild(pid, Signal::SIGKILL);
                        exit_token.cancel();
                        // Reap the zombie: the pidfd future was dropped by the
                        // timeout so waitpid never ran. The thread survives
                        // cancellation and will reap once SIGKILL takes effect.
                        Self::spawn_waitpid_thread(move || {
                            Self::wait_for_exit_code_blocking(pid);
                        });
                    }
                } else {
                    closure.await;
                }
                // Tells the users of the token that the process is gone.
                match oom_watcher {
                    Some(oom_watcher) => oom_watcher.stop(),
                    None => exit_token.cancel(),
                }

                // The exit file tells that the log is complete, so let the read
                // loops write what the process left in the pipes.
                if !exit_paths.is_empty() {
                    Self::wait_for_output(io.attach().await).await;
                }

                let exit_channel_data = ExitChannelData {
                    exit_code,
                    timed_out,
                };
                debug!(?exit_paths, oomed, "Write to exit paths");
                Self::write_to_exit_paths(exit_code, &exit_paths).await;

                if !cleanup_cmd_raw.is_empty() {
                    Self::spawn_cleanup_process(&cleanup_cmd_raw);
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

    fn spawn_cleanup_process(raw_cmd: &[String]) {
        let mut cleanup_cmd = Command::new(&raw_cmd[0]);
        init::reset_oom_score(&mut cleanup_cmd);

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
        // The readiness comes from epoll and the wait uses WNOHANG, so the
        // descriptor itself does not need to be non-blocking.
        let owned_fd = pidfd_open(Pid::from_raw(pid as pid_t)).ok()?;

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

        let reaping = reap_guard();
        let status = waitpid(Pid::from_raw(pid as pid_t), WaitPidFlag::WNOHANG.into());
        drop(reaping);
        match status {
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

    /// Wait for the exit without reaping, which then happens under the
    /// [`reap_guard`].
    fn wait_for_exit_code_blocking(pid: u32) -> i32 {
        const FAILED_EXIT_CODE: i32 = -3;
        let pid = Pid::from_raw(pid as pid_t);
        loop {
            match waitid(Id::Pid(pid), WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT) {
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(Errno::ECHILD) => {
                    debug!("PID {pid} got reaped already");
                    return FAILED_EXIT_CODE;
                }
                Err(err) => {
                    error!("Unable to wait for pid {pid}: {:#}", err);
                    return FAILED_EXIT_CODE;
                }
            }
            let reaping = reap_guard();
            let status = waitpid(pid, Some(WaitPidFlag::WNOHANG));
            drop(reaping);
            match status {
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
                if let Err(e) = Self::write_exit_file(path_buf, &code_str).await {
                    error!("Could not write exit file: {:#}", e);
                }
            }
            .instrument(span)
            .await;
        }
    }

    /// Write the exit file at once, like conmon does. CRI-O reads it as soon
    /// as it gets created, which must not find it empty. CRI-O ignores the
    /// temporary file, which is not named like a container.
    async fn write_exit_file(path: &Path, contents: &str) -> Result<()> {
        let name = path.file_name().context("exit path has no file name")?;
        let mut tmp_name = OsString::from(".");
        tmp_name.push(name);
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        let result = match fs::write(&tmp, contents).await {
            Ok(()) => fs::rename(&tmp, path)
                .await
                .with_context(|| format!("rename to {}", path.display())),
            Err(e) => Err(e).with_context(|| format!("write {}", tmp.display())),
        };
        if result.is_err()
            && let Err(e) = fs::remove_file(&tmp).await
        {
            debug!("Unable to remove {}: {e}", tmp.display());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        container_io::{DRAIN_MAX_WAIT, DRAIN_TIMEOUT, Message},
        container_log::ContainerLog,
        process::Process,
    };
    use nix::sys::signal::kill;
    use std::process::Command;

    /// Spawn a child which exits right away and is never waited for.
    fn spawn_zombie() -> Result<u32> {
        let child = Command::new("true").spawn()?;
        let pid = child.id();
        std::mem::forget(child);
        let start = Instant::now();
        while !is_zombie(pid) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "child never became a zombie"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(pid)
    }

    /// Set for the test process which runs the sweep test.
    const SWEEP_TEST_ENV: &str = "CONMONRS_SWEEP_TEST";

    /// The sweep reaps every unclaimed zombie of the test process, so it would
    /// take the children of parallel tests, and their zombies would make it
    /// flaky. Run it in a process of its own.
    #[test]
    fn sweep_reaps_unclaimed_zombies_after_grace() -> Result<()> {
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "child_reaper::tests::sweep_in_own_process",
                "--ignored",
            ])
            .env(SWEEP_TEST_ENV, "1")
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
        assert!(stdout.contains("1 passed"), "{stdout}");
        Ok(())
    }

    #[tokio::test]
    #[ignore = "run by sweep_reaps_unclaimed_zombies_after_grace in a process of its own"]
    async fn sweep_in_own_process() -> Result<()> {
        if std::env::var_os(SWEEP_TEST_ENV).is_none() {
            return Ok(());
        }
        let reaper = ChildReaper::default();
        let orphan = spawn_zombie()?;
        let claimed = spawn_zombie()?;
        lock(&reaper.pending)?.insert(claimed);

        // Both ways of listing the children find them.
        assert!(ChildReaper::zombie_children().0.contains(&orphan));
        assert!(ChildReaper::children_by_scan().contains(&orphan));

        // The first sweep only records the candidates.
        let mut candidates = HashMap::new();
        reaper
            .sweep_orphans(&mut candidates, Duration::from_secs(60))
            .await;
        assert!(is_zombie(orphan));

        // Without grace the unclaimed one is reaped, the claimed one stays.
        reaper.sweep_orphans(&mut candidates, Duration::ZERO).await;
        assert!(!is_zombie(orphan));
        assert!(is_zombie(claimed));
        assert!(!candidates.contains_key(&orphan));

        waitpid(Pid::from_raw(claimed as pid_t), None)?;
        assert!(!ChildReaper::zombie_children().0.contains(&claimed));
        Ok(())
    }

    /// Wait in real time until the condition holds, without blocking the
    /// runtime of the test.
    async fn wait_until(f: impl Fn() -> bool) {
        for _ in 0..1000 {
            if f() {
                return;
            }
            task::spawn_blocking(|| thread::sleep(std::time::Duration::from_millis(10)))
                .await
                .ok();
        }
        panic!("condition never became true");
    }

    fn is_gone(pid: u32) -> bool {
        kill(Pid::from_raw(pid as pid_t), None).is_err()
    }

    /// A shell which nobody but the watcher waits for. It gets killed through
    /// a pidfd if a test fails before it ended, which cannot hit a reused PID.
    struct Spawned {
        pid: u32,
        process: Process,
    }

    impl Drop for Spawned {
        fn drop(&mut self) {
            let _ = self.process.signal(Signal::SIGKILL);
        }
    }

    fn spawn_sh(script: &str) -> Result<Spawned> {
        let pid = Command::new("sh").args(["-c", script]).spawn()?.id();
        // Not reaped yet, so the PID is still its own.
        let process = Process::open(Pid::from_raw(pid as pid_t));
        Ok(Spawned { pid, process })
    }

    fn child(
        pid: u32,
        timeout: Option<Instant>,
        exit_path: &Path,
        cleanup: &[&str],
    ) -> Result<Child> {
        Ok(Child {
            id: "id".into(),
            kind: ChildKind::Container,
            pid,
            exit_paths: vec![exit_path.into()],
            oom_exit_paths: vec![],
            timeout,
            io: SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?),
            cleanup_cmd: cleanup.iter().map(|s| s.to_string()).collect(),
            token: CancellationToken::new(),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn watch_reports_exit_codes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for (script, expected) in [("exit 3", 3), ("kill -9 $$", 137), ("exit 0", 0)] {
            let exit_path = dir.path().join("exit");
            let sh = spawn_sh(script)?;
            let (_, mut reapable) =
                ReapableChild::from_child(child(sh.pid, None, &exit_path, &[])?);
            let (mut exit_rx, _) = reapable.watch();

            let exit = exit_rx.recv().await?;
            assert_eq!(exit.exit_code, expected, "{script}");
            assert!(!exit.timed_out);
            assert_eq!(std::fs::read_to_string(&exit_path)?, expected.to_string());
            // The token tells about the exit also without an OOM watcher.
            assert!(reapable.token().is_cancelled());
        }
        // No temporary exit file is left behind.
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn write_exit_file_replaces_at_once() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("id");
        ReapableChild::write_exit_file(&path, "1").await?;
        ReapableChild::write_exit_file(&path, "137").await?;
        assert_eq!(std::fs::read_to_string(&path)?, "137");
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);

        // A missing directory fails without leaving anything behind.
        let missing = dir.path().join("missing").join("id");
        assert!(ReapableChild::write_exit_file(&missing, "1").await.is_err());
        assert!(
            ReapableChild::write_exit_file(Path::new("/"), "1")
                .await
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn forget_grandchild_drops_empty_entries() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let exit_path = dir.path().join("exit");
        let reapable = |pid| -> Result<ReapableChild> {
            Ok(ReapableChild::from_child(child(pid, None, &exit_path, &[])?).1)
        };
        let map = Arc::new(Mutex::new(HashMap::from([
            ("a".into(), vec![reapable(1)?, reapable(2)?]),
            ("b".into(), vec![reapable(3)?]),
        ])));

        ChildReaper::forget_grandchild(&map, 3)?;
        ChildReaper::forget_grandchild(&map, 1)?;
        // Unknown PIDs are ignored.
        ChildReaper::forget_grandchild(&map, 4)?;

        let map = lock(&map)?;
        assert_eq!(map.len(), 1);
        assert_eq!(map["a"].iter().map(|c| c.pid).collect::<Vec<_>>(), [2]);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn watch_kills_on_timeout() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let exit_path = dir.path().join("exit");
        let cleaned = dir.path().join("cleaned");
        let cleanup = ["touch", cleaned.to_str().context("path")?];

        let sh = spawn_sh("exec sleep 60")?;
        let pid = sh.pid;
        let timeout = Instant::now() + Duration::from_secs(1);
        let (_, mut reapable) =
            ReapableChild::from_child(child(pid, Some(timeout), &exit_path, &cleanup)?);
        let (mut exit_rx, _) = reapable.watch();

        let exit = exit_rx.recv().await?;
        assert_eq!(exit.exit_code, -3);
        assert!(exit.timed_out);
        assert_eq!(std::fs::read_to_string(&exit_path)?, "-3");
        wait_until(|| is_gone(pid)).await;
        wait_until(|| cleaned.exists()).await;
        Ok(())
    }

    #[tokio::test]
    async fn failed_create_cleanup_spares_claimed_pid() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let pidfile = dir.path().join("pid");
        let sh = spawn_sh("exec sleep 60")?;
        let pid = sh.pid;
        std::fs::write(&pidfile, pid.to_string())?;

        // A stale PID file naming a process this server waits for.
        lock(&reaper.pending)?.insert(pid);
        reaper.cleanup_failed_grandchild(&pidfile).await;
        assert!(!is_gone(pid) && !is_zombie(pid));

        lock(&reaper.pending)?.remove(&pid);
        reaper.cleanup_failed_grandchild(&pidfile).await;
        wait_until(|| is_gone(pid)).await;
        Ok(())
    }

    #[tokio::test]
    async fn registration_keeps_the_child_claimed() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let sh = spawn_sh("exec sleep 60")?;
        // As `create_child` leaves it.
        lock(&reaper.pending)?.insert(sh.pid);
        assert!(reaper.is_claimed(sh.pid));

        let child = child(sh.pid, None, &dir.path().join("exit"), &[])?;
        reaper.watch_grandchild(child, vec![], InFlight(None))?;
        assert!(reaper.is_claimed(sh.pid));
        assert!(!lock(&reaper.pending)?.contains(&sh.pid));
        Ok(())
    }

    #[tokio::test]
    async fn registration_after_shutdown_kills_child() -> Result<()> {
        let reaper = ChildReaper::default();
        reaper.kill_grandchildren(Signal::SIGKILL).await?;

        // A call which was in progress while the shutdown killed the others.
        let dir = tempfile::tempdir()?;
        let sh = spawn_sh("exec sleep 60")?;
        let pid = sh.pid;
        assert!(
            reaper
                .watch_grandchild(
                    child(pid, None, &dir.path().join("exit"), &[])?,
                    vec![],
                    InFlight(None)
                )
                .is_err()
        );
        wait_until(|| is_gone(pid)).await;

        // New calls do not start at all.
        let mut io = ContainerIO::new(false, ContainerLog::new())?;
        let res = reaper
            .create_child(
                "true",
                ["x"],
                false,
                &mut io,
                &dir.path().join("pid"),
                None,
                vec![],
                false,
                vec![],
            )
            .await;
        assert!(res.is_err());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn late_oom_result_after_timeout_counts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let oom_path = dir.path().join("oom");
        let paths = [oom_path.clone()];

        // The watcher still confirms an event while the timeout passes.
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        task::spawn(async move {
            time::sleep(Duration::from_secs(1)).await;
            tx.send(OOMEvent::possible(true)).await
        });
        assert!(ReapableChild::late_oom(&mut rx, 137, &paths).await);
        assert!(oom_path.exists());

        // A normal exit is none, and no result at all either.
        std::fs::remove_file(&oom_path)?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(OOMEvent::possible(true)).await?;
        assert!(!ReapableChild::late_oom(&mut rx, 0, &paths).await);
        let (_tx, mut rx) = tokio::sync::mpsc::channel(1);
        let start = Instant::now();
        assert!(!ReapableChild::late_oom(&mut rx, 137, &paths).await);
        assert_eq!(start.elapsed(), ReapableChild::OOM_RESULT_WAIT);
        assert!(!oom_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn runtime_output_with_terminal_does_not_block() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let mut io = ContainerIO::new(true, ContainerLog::new())?;

        // More than a pipe holds, the error comes last.
        let script = "head -c 300000 /dev/zero | tr '\\0' x; head -c 300000 /dev/zero >&2; \
                      echo the-error >&2; exit 1";
        let start = Instant::now();
        let res = time::timeout(
            Duration::from_secs(20),
            reaper.create_child(
                "sh",
                ["-c", script],
                false,
                &mut io,
                &dir.path().join("pid"),
                Some(Instant::now() + Duration::from_secs(10)),
                vec![],
                false,
                vec![],
            ),
        )
        .await?;
        let err = format!("{:#}", res.err().context("runtime failure expected")?);
        assert!(err.contains("exited with: 1"), "{err}");
        assert!(err.trim_end().ends_with("the-error"));
        assert!(err.len() < 2 * RuntimeOutput::MAX_STDERR);
        assert!(start.elapsed() < Duration::from_secs(10));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_calls_in_progress() -> Result<()> {
        let reaper = Arc::new(ChildReaper::default());
        let call = reaper.begin_call()?;
        let shutdown = task::spawn({
            let reaper = reaper.clone();
            async move { reaper.kill_grandchildren(Signal::SIGKILL).await }
        });
        time::sleep(Duration::from_secs(1)).await;
        assert!(!shutdown.is_finished());
        // No new call starts meanwhile.
        assert!(reaper.begin_call().is_err());

        drop(call);
        shutdown.await??;

        // A call which never ends does not keep the shutdown forever.
        let _call = InFlight(Some(reaper.in_flight.clone()));
        reaper.in_flight.send_modify(|n| *n += 1);
        let start = Instant::now();
        reaper.kill_grandchildren(Signal::SIGKILL).await?;
        assert_eq!(start.elapsed(), ChildReaper::IN_FLIGHT_WAIT);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn exit_files_wait_for_quiet_output() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let output = attach.output();

        // Nothing flows, a process holds the pipes open.
        let start = Instant::now();
        ReapableChild::wait_for_output(attach.clone()).await;
        assert_eq!(start.elapsed(), DRAIN_TIMEOUT);

        // Output which keeps flowing is bounded.
        let writer = task::spawn({
            let attach = attach.clone();
            async move {
                loop {
                    attach.write(Message::Data(
                        "data".into(),
                        crate::container_io::Pipe::StdOut,
                    ));
                    time::sleep(DRAIN_TIMEOUT / 2).await;
                }
            }
        });
        let start = Instant::now();
        ReapableChild::wait_for_output(attach.clone()).await;
        assert_eq!(start.elapsed(), DRAIN_MAX_WAIT);
        writer.abort();

        // Ended output does not wait at all.
        drop(output);
        let start = Instant::now();
        ReapableChild::wait_for_output(attach).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        Ok(())
    }
}
