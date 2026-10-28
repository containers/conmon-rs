//! Child process reaping and management.
use crate::fd_mapping::{CommandFdExt, FdMapping};
use crate::{
    async_fd,
    attach::SharedContainerAttach,
    child::{Child, ChildKind, sleep_until},
    container_io::{
        ContainerIO, ContainerIOType, DRAIN_MAX_LOG_WAIT, Drain, Message, SharedContainerIO,
    },
    init,
    mutex::lock,
    oom_watcher::{OOMEvent, OOMWatcher},
    process::{
        Process, child_pid, children_by_scan, is_zombie, pidfd_open, read_pid_file, reap_guard,
    },
    tail::Tail,
};
use anyhow::{Context, Result, bail, format_err};
use futures::future::join_all;
use nix::{
    errno::Errno,
    sys::{
        signal::{Signal, killpg},
        wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid},
    },
    unistd::Pid,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsString,
    io,
    ops::{Deref, DerefMut},
    os::fd::{AsFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    pin::pin,
    process::Stdio,
    str,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, Interest},
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
pub(crate) struct RuntimeOutput {
    stderr: Arc<Mutex<StderrTail>>,
    task: JoinHandle<()>,
}

type StderrTail = Tail<{ RuntimeOutput::MAX_STDERR }>;

impl RuntimeOutput {
    /// How much of the end of stderr is kept.
    const MAX_STDERR: usize = 64 * 1024;

    pub(crate) fn spawn(stdout: Option<ChildStdout>, stderr: Option<ChildStderr>) -> Self {
        let buf = Arc::<Mutex<StderrTail>>::default();
        let stderr_buf = buf.clone();
        let task = task::spawn(async move {
            tokio::join!(
                async {
                    if let Some(stdout) = stdout {
                        Self::read(stdout, None).await;
                    }
                },
                async {
                    if let Some(stderr) = stderr {
                        Self::read(stderr, Some(&stderr_buf)).await;
                    }
                },
            );
        });
        Self { stderr: buf, task }
    }

    /// Read until the end, keeping the end in `buf` if any.
    async fn read(mut reader: impl AsyncRead + Unpin, buf: Option<&Mutex<StderrTail>>) {
        let mut chunk = [0; 8192];
        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let Some(buf) = buf else {
                continue;
            };
            lock(buf).push(&chunk[..n]);
        }
    }

    /// Wait for the end of the output for a while, a helper of the runtime
    /// may hold the pipes open, and return the end of stderr.
    pub(crate) async fn finish(output: Option<Self>) -> Vec<u8> {
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
        std::mem::take(&mut *lock(&output.stderr)).into_end()
    }
}

#[derive(Debug, Default)]
pub struct ChildReaper {
    grandchildren: Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>>,

    /// The runtimes and container processes of calls in progress, which are
    /// not registered as grandchildren yet. Their exit status belongs to the
    /// call, so the orphan sweep leaves them alone.
    pending: Arc<Mutex<HashSet<Pid>>>,

    /// Wakes the orphan sweep up early, for example after a failed runtime
    /// call or on shutdown.
    sweep: Notify,

    /// Set by the shutdown while it holds the lock of the grandchildren, so a
    /// child registered later knows that the shutdown missed it.
    shutting_down: AtomicBool,

    /// The runtime calls in progress, including the registration of their
    /// child, which the shutdown waits for.
    in_flight: Arc<watch::Sender<Calls>>,

    /// The watchers of children registered after the shutdown took its
    /// snapshot, which it waits for as well.
    late_watchers: Mutex<Vec<(Pid, TaskHandle)>>,
}

/// Counts a runtime call as in progress until dropped. A successful
/// [`ChildReaper::create_child`] hands it to the caller, which passes it to
/// [`ChildReaper::watch_grandchild`], so the call ends with the registration
/// of its child, or wherever the caller gives up.
#[derive(Debug)]
#[cfg_attr(test, derive(Default))]
pub struct InFlight {
    calls: Option<Arc<watch::Sender<Calls>>>,

    /// The number of the call, see [`Calls`].
    seq: u64,

    /// Keeps the process which the call started pending until the call
    /// ends, also if the call gets dropped before its registration.
    pending: Option<PendingPid>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(calls) = &self.calls {
            calls.send_modify(|calls| {
                calls.in_progress.remove(&self.seq);
            });
        }
    }
}

/// The runtime calls in progress, numbered in the order they started.
#[derive(Debug, Default)]
struct Calls {
    /// The number of the next call.
    next: u64,

    /// Until when each call in progress may still claim a process, by its
    /// number: the deadline of its runtime and the
    /// [`ChildReaper::CLAIM_MARGIN`]. None for a call without a deadline.
    in_progress: BTreeMap<u64, Option<Instant>>,
}

impl Calls {
    /// Whether a call numbered below `before` is in progress, which may
    /// still claim a process at `now`. The deadlines of the calls differ, an
    /// older one may still claim one while a younger one cannot anymore.
    fn may_claim_before(&self, before: u64, now: Instant) -> bool {
        self.in_progress
            .range(..before)
            .any(|(_, claim_by)| claim_by.is_none_or(|claim_by| now < claim_by))
    }
}

/// A zombie which nobody claimed, see [`ChildReaper::sweep_orphans`].
#[derive(Clone, Copy, Debug)]
struct Candidate {
    first_seen: Instant,

    /// The calls numbered below this started before it was first seen. It
    /// may be the process of one of them, which may still claim it.
    calls_before: u64,
}

/// Marks a PID as pending for the lifetime of the guard.
#[derive(Debug)]
struct PendingPid {
    pending: Arc<Mutex<HashSet<Pid>>>,
    pid: Pid,
}

impl PendingPid {
    fn new(pending: &Arc<Mutex<HashSet<Pid>>>, pid: Pid) -> Self {
        lock(pending).insert(pid);
        Self {
            pending: pending.clone(),
            pid,
        }
    }
}

impl Drop for PendingPid {
    fn drop(&mut self) {
        lock(&self.pending).remove(&self.pid);
    }
}

/// first usable file descriptor after stdin, stdout and stderr
const FIRST_FD_AFTER_STDIO: RawFd = 3;

/// How [`ChildReaper::create_child`] runs a runtime.
#[derive(Debug)]
pub struct RuntimeCall<'a> {
    /// The binary of the runtime.
    pub runtime: &'a Path,
    pub args: &'a [String],

    /// Whether the process gets a stdin pipe.
    pub stdin: bool,

    /// Where the runtime writes the PID of the process it started.
    pub pidfile: &'a Path,

    /// When a runtime which did not finish gets killed, never without one.
    pub deadline: Option<Instant>,
    pub env_vars: Vec<(String, String)>,

    /// Descriptors the runtime gets from 3 on, in this order.
    pub additional_fds: Vec<OwnedFd>,
}

impl<'a> RuntimeCall<'a> {
    /// A call without stdin, deadline, environment or descriptors.
    pub fn new(runtime: &'a Path, args: &'a [String], pidfile: &'a Path) -> Self {
        Self {
            runtime,
            args,
            stdin: false,
            pidfile,
            deadline: None,
            env_vars: vec![],
            additional_fds: vec![],
        }
    }
}

impl ChildReaper {
    /// How long to wait for the console connection of a runtime which already
    /// exited successfully.
    const CONNECT_AFTER_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Retrieve the container registered under `id`. Execs share the id of
    /// their container and are skipped. The error starts with
    /// [`CONTAINER_NOT_FOUND`] if there is none.
    pub fn get(&self, id: &str) -> Result<ReapableChild> {
        let lock = lock(&self.grandchildren);
        let r = lock
            .get(id)
            .and_then(|v| v.iter().find(|c| c.kind == ChildKind::Container))
            .with_context(|| format!("{CONTAINER_NOT_FOUND}: {id}"))?
            .clone();
        Ok(r)
    }

    /// Run the runtime with the I/O of the process it starts, and return the
    /// PID of the process once the runtime exited. The caller registers it
    /// through [`ChildReaper::watch_grandchild`].
    pub async fn create_child(
        &self,
        call: RuntimeCall<'_>,
        container_io: &mut ContainerIO,
    ) -> Result<(Pid, CancellationToken, InFlight)> {
        let RuntimeCall {
            runtime,
            args,
            stdin,
            pidfile,
            deadline: runtime_deadline,
            env_vars,
            additional_fds,
        } = call;
        let mut in_flight = self.begin_call(runtime_deadline)?;
        // A PID file left from before, like of a container created in the
        // same bundle earlier, would name a process which the cleanup of a
        // failed call kills, see `cleanup_failed_grandchild`.
        match std::fs::remove_file(pidfile) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(e).context("remove stale PID file");
            }
            _ => {}
        }
        debug!("Running: {:?} {:?}", runtime, args);
        let mut cmd = Command::new(runtime);
        // A runtime which gets dropped, for example on a panic, must not keep
        // running unobserved.
        cmd.kill_on_drop(true);
        // In a process group of its own, which a runtime that hangs gets
        // killed with, see `stop_runtime`. Signals for the group of the
        // server, like a SIGINT from a terminal, do not reach it, the
        // server stops it on its own.
        cmd.process_group(0);

        // Without one it must not inherit the stdin of the server, which the
        // runtime would pass on to the container.
        cmd.stdin(if stdin { Stdio::piped() } else { Stdio::null() });

        cmd.args(args)
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
            )?;
        // After the mappings: the pre_exec hooks run in the order they got
        // added, and this one lowers the open files limit to the one the
        // server started with, below which the mappings may have to move a
        // descriptor of the server.
        init::reset_inherited(&mut cmd);
        let mut child = Runtime(cmd.spawn().context("spawn child process")?);

        // The mappings own the passed descriptors and live in the command's
        // pre_exec hook, dropping the command closes them in the parent.
        drop(cmd);

        // Not waited for yet, so it has its PID. Dropping it on a failure
        // kills it.
        let runtime_pid = child_pid(child.id()).context("PID of runtime")?;
        let _runtime_pending = PendingPid::new(&self.pending, runtime_pid);

        debug!("Running child on PID: {runtime_pid}");

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
                return Err(self.fail_create(pidfile, e, &stderr));
            }
        };

        // The container process got reparented to this server once the
        // runtime exited. Read synchronously and mark it as pending right
        // away, before waiting for the output of the runtime, which a helper
        // may hold open for a while. A process which exits before it is
        // registered must not be taken for an orphan. It is registered by the
        // caller through `watch_grandchild`, which ends the call and with it
        // the pending mark, like a caller which gives up before that does.
        let grandchild_pid = status.success().then(|| read_pid_file(pidfile));
        if let Some(Ok(pid)) = &grandchild_pid {
            in_flight.pending = Some(PendingPid::new(&self.pending, *pid));
        }
        let stderr = RuntimeOutput::finish(output).await;

        let error = match grandchild_pid {
            Some(Ok(pid)) => return Ok((pid, token, in_flight)),
            Some(Err(e)) => e,
            None => match status.code() {
                Some(code) => format_err!("child command exited with: {code}"),
                None => format_err!("child command exited with signal"),
            },
        };
        // Nothing registers the process, so the stdin loops have to be
        // stopped here. The caller stops the output once it took the error of
        // the runtime, see `ContainerIO::runtime_error`.
        token.cancel();
        Err(self.fail_create(pidfile, error, &stderr))
    }

    /// Start a runtime call whose runtime gets killed at the `deadline`,
    /// unless the server shuts down. The shutdown waits for the calls in
    /// progress, see [`ChildReaper::kill_grandchildren`].
    fn begin_call(&self, deadline: Option<Instant>) -> Result<InFlight> {
        // Under the lock the shutdown takes its snapshot with, so that it
        // either sees this call or this call sees the shutdown.
        let _map = lock(&self.grandchildren);
        if self.shutting_down.load(Ordering::SeqCst) {
            bail!("server is shutting down")
        }
        let claim_by = deadline.and_then(|deadline| deadline.checked_add(Self::CLAIM_MARGIN));
        let mut seq = 0;
        self.in_flight.send_modify(|calls| {
            seq = calls.next;
            calls.next += 1;
            calls.in_progress.insert(seq, claim_by);
        });
        Ok(InFlight {
            calls: Some(self.in_flight.clone()),
            seq,
            pending: None,
        })
    }

    /// Stop a runtime which failed before it exited: give it `grace` to exit
    /// on its own, so that it still writes the PID file the cleanup needs,
    /// kill it otherwise, and return its stderr.
    async fn stop_runtime(
        mut child: Runtime,
        output: Option<RuntimeOutput>,
        grace: Duration,
    ) -> Vec<u8> {
        if time::timeout(grace, child.wait()).await.is_err() {
            debug!("Runtime did not exit in time, killing it");
            // Its helpers in its process group go with it, like hooks or a
            // runc init which did not set up the container yet, which would
            // be left running otherwise. A container init which started a
            // session of its own syncs with its runtime over a socket, and
            // fails once that closes. The runtime was not reaped yet, so the
            // ID of the group is still its own.
            if let Ok(pid) = child_pid(child.id())
                && let Err(e) = killpg(pid, Signal::SIGKILL)
            {
                debug!("Unable to kill process group of runtime: {e}");
            }
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
    fn fail_create(&self, pidfile: &Path, error: anyhow::Error, stderr: &[u8]) -> anyhow::Error {
        // Do not leave a container process behind which the runtime created
        // before it failed, and reap what else it left.
        self.cleanup_failed_grandchild(pidfile);
        self.sweep.notify_one();

        let stderr = String::from_utf8_lossy(stderr);
        let stderr = stderr.trim();
        let error = if stderr.is_empty() {
            error
        } else {
            format_err!("{error:#}: {stderr}")
        };
        // The caller logs the error it returns.
        debug!("Failed: {error:#}");
        error
    }

    /// How long a zombie nobody claimed stays unreaped before the sweep takes
    /// it, so that an owner which is about to wait for it still can.
    const ORPHAN_GRACE: Duration = Duration::from_secs(1);

    /// How long the sweep leaves a zombie nobody claimed alone at most, for
    /// the runtime calls which were in progress when it was first seen. The
    /// process of a call can exit before the call claims it, like an exec
    /// which ends before its runtime exited, and its exit status belongs to
    /// the call. Each of these calls defers the zombie only until its own
    /// runtime deadline and the [`ChildReaper::CLAIM_MARGIN`]. This bound
    /// counts from when the zombie was first seen, the earlier of both ends
    /// the deferral. That is the one of the call for a zombie first seen
    /// late in its call, like the process of a slow exec runtime which
    /// exited right before it, and this one for a create, whose runtime gets
    /// 10 minutes: a runtime which hangs must not keep the zombies an init
    /// of a PID namespace waits for that long. An exec whose process exits
    /// more than this before its runtime does loses its exit code.
    const ORPHAN_MAX_DEFER: Duration = Duration::from_secs(30);

    /// How long after the deadline of its runtime a call may still claim
    /// its process: a runtime which exits right before it may still take
    /// [`ChildReaper::CONNECT_AFTER_EXIT_TIMEOUT`] to connect the terminal,
    /// the [`ChildReaper::ORPHAN_GRACE`] on top covers the scheduling.
    const CLAIM_MARGIN: Duration =
        Self::CONNECT_AFTER_EXIT_TIMEOUT.saturating_add(Self::ORPHAN_GRACE);

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
    /// registered nor pending for [`ChildReaper::ORPHAN_GRACE`] is an orphan,
    /// once no runtime call which may still claim it is in progress, at most
    /// for [`ChildReaper::ORPHAN_MAX_DEFER`]. Once the server shuts down,
    /// those calls cannot register a process anymore and are not waited for.
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
                        () = self.sweep.notified() => {}
                        // Only candidates need a recheck once their grace is over.
                        () = time::sleep(Self::ORPHAN_GRACE), if !candidates.is_empty() => {}
                    }
                    self.sweep_orphans(&mut candidates, Self::ORPHAN_GRACE, Self::ORPHAN_MAX_DEFER)
                        .await;
                }
            }
            .instrument(debug_span!("orphan_sweep")),
        );
    }

    /// Reap the zombie children which have been unclaimed for `grace`, and
    /// up to `max_defer` while a call which was in progress when the zombie
    /// was first seen still is and may still claim it, unless the server
    /// shuts down. `candidates` remembers when a zombie was first seen and
    /// the calls which were started then.
    async fn sweep_orphans(
        &self,
        candidates: &mut HashMap<Pid, Candidate>,
        grace: Duration,
        max_defer: Duration,
    ) {
        // Most children get reaped by their owners right away, without a
        // zombie left the scan is not needed.
        if !Self::has_zombie() {
            candidates.clear();
            return;
        }
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
        // The owner of a claimed zombie reaps it soon, it needs no recheck.
        let claimed = self.claimed();
        // After the scan, a call which started later did not start the zombie.
        let calls_before = self.in_flight.borrow().next;
        for pid in zombies {
            if claimed.contains(&pid) {
                candidates.remove(&pid);
                continue;
            }
            let candidate = *candidates.entry(pid).or_insert(Candidate {
                first_seen: now,
                calls_before,
            });
            if now.duration_since(candidate.first_seen) < grace
                || self.defers(candidate, now, max_defer)
            {
                continue;
            }
            // Checked right before reaping, the sets change while the scan
            // runs. After the calls: a call which claimed the zombie
            // meanwhile is either still in progress or registered it.
            if self.is_claimed(pid) {
                candidates.remove(&pid);
                continue;
            }
            let reaping = reap_guard();
            let status = waitpid(pid, Some(WaitPidFlag::WNOHANG));
            drop(reaping);
            match status {
                Ok(status) => debug!("Reaped orphaned child {pid}: {status:?}"),
                Err(e) => debug!("Unable to reap orphaned child {pid}: {e}"),
            }
            candidates.remove(&pid);
        }
    }

    /// Whether the sweep leaves the zombie `candidate` alone at `now` for a
    /// call which may still claim it, at most for `max_defer` since it was
    /// first seen, see [`ChildReaper::ORPHAN_MAX_DEFER`].
    fn defers(&self, candidate: Candidate, now: Instant, max_defer: Duration) -> bool {
        !self.shutting_down.load(Ordering::SeqCst)
            && now.duration_since(candidate.first_seen) < max_defer
            && self
                .in_flight
                .borrow()
                .may_claim_before(candidate.calls_before, now)
    }

    /// Whether runtime calls are in progress.
    #[cfg(test)]
    fn calls_in_progress(&self) -> bool {
        !self.in_flight.borrow().in_progress.is_empty()
    }

    /// Whether a child of any thread of this process exited and was not
    /// reaped yet, without reaping it.
    fn has_zombie() -> bool {
        !matches!(
            waitid(
                Id::All,
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT
            ),
            Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD)
        )
    }

    /// Whether a watcher or a call in progress waits for the child.
    ///
    /// Both sets get checked under the lock of the grandchildren, which
    /// `watch_grandchild` holds while it moves a child from one to the other.
    fn is_claimed(&self, pid: Pid) -> bool {
        let map = lock(&self.grandchildren);
        map.values().flatten().any(|child| child.pid == pid) || lock(&self.pending).contains(&pid)
    }

    /// The children which a watcher or a call in progress waits for, see
    /// [`ChildReaper::is_claimed`].
    fn claimed(&self) -> HashSet<Pid> {
        let map = lock(&self.grandchildren);
        let mut claimed: HashSet<_> = map.values().flatten().map(|child| child.pid).collect();
        claimed.extend(lock(&self.pending).iter());
        claimed
    }

    /// The zombie children of this process, and whether the list is
    /// complete, see [`Self::children`].
    fn zombie_children() -> (Vec<Pid>, bool) {
        let (children, complete) = Self::children();
        let zombies = children.into_iter().filter(|pid| is_zombie(*pid)).collect();
        (zombies, complete)
    }

    /// The children of all threads of this process. The kernel lists them in
    /// `/proc/self/task/<tid>/children`, which needs `CONFIG_PROC_CHILDREN`,
    /// fall back to scanning all processes without it. The list is not
    /// complete if a thread exited while listing them.
    fn children() -> (Vec<Pid>, bool) {
        static PROC_CHILDREN: LazyLock<bool> =
            LazyLock::new(|| Path::new("/proc/thread-self/children").exists());
        if !*PROC_CHILDREN {
            return (children_by_scan(), true);
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
                    .filter_map(|pid| pid.parse().ok())
                    .map(Pid::from_raw),
            );
        }
        (children, complete)
    }

    /// Kill and reap the container process of a failed runtime call, if the
    /// runtime got as far as writing its PID. It got reparented to this
    /// subreaper and nobody else waits for it.
    ///
    /// Only this PID is waited for. Reaping any exited child instead would take
    /// the exit status of runtimes of concurrent calls, or of grandchildren
    /// which are not registered yet.
    fn cleanup_failed_grandchild(&self, pidfile: &Path) {
        let Ok(pid) = read_pid_file(pidfile) else {
            return;
        };
        // A stale PID file may name a process which got reused by a container
        // or exec of this server, which must not be killed.
        if self.is_claimed(pid) {
            debug!("Not cleaning up PID {pid} of failed runtime call, it is in use");
            return;
        }

        // Only a child, the PID may have been reused otherwise.
        if let Err(e) = Process::child(pid).signal(Signal::SIGKILL) {
            debug!("Not cleaning up PID {pid} of failed runtime call: {e}");
            return;
        }
        debug!("Cleaning up PID {pid} of failed runtime call");
        task::spawn(ReapableChild::wait_for_exit_code_async(
            CancellationToken::new(),
            pid,
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
        let seq = reapable_grandchild.seq;

        let mut map = lock(&self.grandchildren);
        // Under the lock, so that the child is registered before it can be
        // forgotten. A task which never runs, like one spawned while the
        // runtime shuts down, does not forget it, see `watch`.
        let grandchildren = self.grandchildren.clone();
        let key = id.clone();
        let exit_rx = reapable_grandchild.watch(move || {
            drop(leak_fds);
            Self::forget_grandchild(&grandchildren, &key, seq);
        });
        let shutting_down = self.shutting_down.load(Ordering::SeqCst);
        if shutting_down && let Some(task) = &reapable_grandchild.task {
            lock(&self.late_watchers).push((pid, task.clone()));
        }
        map.entry(id).or_default().push(reapable_grandchild);
        // Ends the pending mark only once registered, so it is claimed all
        // the time, see `is_claimed`.
        drop(call);
        drop(map);

        // A call which was in progress while the shutdown killed the others.
        if shutting_down {
            Self::kill_late_grandchild(pid);
            bail!("server is shutting down")
        }
        Ok(exit_rx)
    }

    /// Kill a grandchild registered after the shutdown killed the others,
    /// unless it is gone already, and return whether it was signalled. The
    /// sweep does not defer orphans while the server shuts down, so the
    /// process may have exited and got reaped before its call claimed it.
    /// Opening a pidfd fails with `ESRCH` then, and neither the PID nor the
    /// group of the process gets signalled. A PID which got reused that fast
    /// only gets signalled if it belongs to an unreaped child of the server,
    /// see [`kill_grandchild`].
    fn kill_late_grandchild(pid: Pid) -> bool {
        if matches!(pidfd_open(pid), Err(Errno::ESRCH)) {
            debug!(%pid, "Not killing grandchild, it got reaped already");
            return false;
        }
        kill_grandchild(pid, Signal::SIGKILL);
        true
    }

    /// Forget the child of the registration `seq` under `id`. Not by its PID,
    /// which may be in use by a new child already.
    fn forget_grandchild(
        locked_grandchildren: &Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>>,
        id: &str,
        seq: u64,
    ) {
        let mut map = lock(locked_grandchildren);
        let Some(children) = map.get_mut(id) else {
            return;
        };
        if let Some(pos) = children.iter().position(|v| v.seq == seq) {
            children.swap_remove(pos);
        }
        // Containers get a new ID on every restart, keeping the entries of
        // the old ones would grow the map forever.
        if children.is_empty() {
            map.remove(id);
        }
    }

    /// Kill every grandchild and wait for their watchers and the runtime
    /// calls in progress until the `deadline` of the shutdown. A process
    /// which does not exit on SIGKILL, like one stuck in the kernel, must not
    /// keep the server from stopping. It fails if they did not end in time.
    pub async fn kill_grandchildren(&self, deadline: Instant) -> Result<()> {
        debug!("Killing grandchildren");
        // Collect only PIDs and task handles to avoid cloning entire ReapableChild
        let children: Vec<_> = {
            let map = lock(&self.grandchildren);
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
            let span = debug_span!("kill_grandchild", %pid);
            let _enter = span.enter();
            debug!("Killing single grandchild");
            kill_grandchild(pid, Signal::SIGKILL);
            tasks.extend(task.map(|task| (pid, task)));
        }
        // An init of a PID namespace only exits once every process of the
        // namespace is reaped, including zombies reparented to this process.
        // The sweep keeps running while the watchers are awaited.
        self.sweep.notify_one();
        let mut left = Self::wait_for_watchers(tasks, deadline).await?;

        // Runtime calls in progress get to finish, the server would kill a
        // runtime halfway otherwise. Their children got killed on registration.
        // A call which hangs does not take the time of the late watchers.
        let mut in_flight = self.in_flight.subscribe();
        let in_flight_deadline = deadline.min(Instant::now() + Self::IN_FLIGHT_WAIT);
        let calls_ended = time::timeout_at(
            in_flight_deadline,
            in_flight.wait_for(|calls| calls.in_progress.is_empty()),
        )
        .await
        .is_ok();
        let late = std::mem::take(&mut *lock(&self.late_watchers));
        left.extend(Self::wait_for_watchers(late, deadline).await?);

        match (left.is_empty(), calls_ended) {
            (true, true) => {
                debug!("Done killing all grandchildren");
                Ok(())
            }
            (true, false) => bail!("runtime calls still in progress"),
            (false, true) => bail!("PIDs {left:?} did not exit"),
            (false, false) => bail!("PIDs {left:?} did not exit, runtime calls still in progress"),
        }
    }

    /// How long the shutdown waits for runtime calls in progress at most,
    /// within its deadline.
    const IN_FLIGHT_WAIT: Duration = Duration::from_secs(5);

    /// Wait for the watchers of the grandchildren until the deadline, and
    /// return the PIDs of the ones which did not end.
    async fn wait_for_watchers(
        tasks: Vec<(Pid, TaskHandle)>,
        deadline: Instant,
    ) -> Result<Vec<i32>> {
        let mut handles = Vec::with_capacity(tasks.len());
        for (pid, task) in tasks {
            if let Some(handle) = lock(&task).take() {
                handles.push((pid, handle));
            }
        }
        let waits = handles.iter_mut().map(|(pid, handle)| {
            async move {
                debug!("Waiting for task to close");
                if let Err(e) = handle.await {
                    error!("Unable to close grandchild: {e:#}");
                }
                debug!("Done killing single grandchild");
            }
            .instrument(debug_span!("close", %pid))
        });
        if time::timeout_at(deadline, join_all(waits)).await.is_ok() {
            return Ok(vec![]);
        }
        Ok(handles
            .iter()
            .filter(|(_, handle)| !handle.is_finished())
            .map(|(pid, _)| pid.as_raw())
            .collect())
    }
}

/// A runtime whose process group gets killed if it is dropped before it got
/// reaped, like a call in progress which the shutdown drops once it stops
/// waiting for it. `kill_on_drop` only kills the runtime, its helpers in its
/// group, like hooks or a runc init which did not set up the container yet,
/// would be left running, see [`ChildReaper::stop_runtime`]. Until it got
/// reaped, the ID of the group is still the one of the runtime.
#[derive(Debug)]
struct Runtime(RuntimeChild);

impl Deref for Runtime {
    type Target = RuntimeChild;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Runtime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // No ID once it got reaped.
        if let Ok(pid) = child_pid(self.0.id())
            && let Err(e) = killpg(pid, Signal::SIGKILL)
        {
            debug!("Unable to kill process group of dropped runtime: {e}");
        }
    }
}

/// Runs a closure once dropped, at the end of a task however it ends.
struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// Signal a grandchild and its process group, unless it got reaped already,
/// see [`Process::signal_group`]. The grandchildren got reparented to this
/// subreaper, so the PID refers to a child.
pub fn kill_grandchild(pid: Pid, s: Signal) {
    if let Err(e) = Process::child(pid).signal_group(s) {
        debug!(%pid, "Not signalling grandchild, it is gone: {e}");
    }
}

type TaskHandle = Arc<Mutex<Option<JoinHandle<()>>>>;

#[derive(Clone, Debug)]
pub struct ReapableChild {
    /// Tells the registrations apart, unlike the PID, which gets reused.
    seq: u64,
    kind: ChildKind,
    exit_paths: Arc<[PathBuf]>,
    oom_exit_paths: Arc<[PathBuf]>,
    pid: Pid,
    io: SharedContainerIO,
    timeout: Option<Instant>,
    token: CancellationToken,
    task: Option<TaskHandle>,
    cleanup_cmd: Arc<[String]>,
}

impl ReapableChild {
    pub fn pid(&self) -> Pid {
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

/// The start of the error of a request for a container the server does not
/// know, clients match it, see `ErrContainerNotFound` of the Go client. The
/// message of older servers is "child not available".
const CONTAINER_NOT_FOUND: &str = "container not found";

/// The exit code of a process whose exit status is not known, because it did
/// neither exit nor get killed by a signal, or waitpid failed after its pidfd
/// got readable.
const EXIT_CODE_UNKNOWN: i32 = -1;

/// The exit code of an exec which could not be started, for example because
/// the runtime failed.
pub const EXIT_CODE_NOT_STARTED: i32 = -2;

/// The exit code of a process which did not exit before its timeout. Clients
/// take -3 for a timeout, no other failure may report it, except for
/// [`EXIT_CODE_WAIT_FAILED`], which always did.
pub const EXIT_CODE_TIMED_OUT: i32 = -3;

/// The exit code of a process whose exit status the blocking wait, used
/// without pidfds, could not get, for example because it got reaped already.
/// It is the same as [`EXIT_CODE_TIMED_OUT`] for compatibility, the pidfd wait
/// reports [`EXIT_CODE_UNKNOWN`] instead.
const EXIT_CODE_WAIT_FAILED: i32 = -3;

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
        oom: impl Future<Output = Option<OOMEvent>>,
        exit_code: i32,
        oom_exit_paths: &[PathBuf],
    ) -> bool {
        let event = time::timeout(Self::OOM_RESULT_WAIT, oom)
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
    /// the exit file. Output on its way into the log is no pause, a slow log
    /// driver must not cut off the end of the log, up to the `deadline`.
    async fn wait_for_output(attach: SharedContainerAttach, deadline: Instant) {
        let (mut rx, ended) = attach.subscribe();
        if ended {
            return;
        }
        let mut drain = Drain::default();
        loop {
            if time::timeout_at(deadline, attach.wait_logged())
                .await
                .is_err()
            {
                warn!("Log drivers did not take the output in time, not waiting for them");
                return;
            }
            // A read loop which got output while the drain waited gives it
            // another chance.
            let Ok(output) = time::timeout_at(deadline, drain.recv_output(&mut rx, false)).await
            else {
                debug!("Output did not end in time, not waiting for it");
                return;
            };
            if output.is_none() && (drain.exhausted() || !attach.is_logging()) {
                return;
            }
        }
    }

    /// Wait for the output like [`ReapableChild::wait_for_output`], then
    /// stop the read loops and wait for them to end, which writes the lines
    /// still waiting for their ending into the log. Nothing reaches the log
    /// after the exit file this way. A log write which hangs must not hold
    /// the exit file back, all of it takes [`DRAIN_MAX_LOG_WAIT`] at most.
    async fn finish_output(attach: SharedContainerAttach) {
        let deadline = Instant::now() + DRAIN_MAX_LOG_WAIT;
        Self::wait_for_output(attach.clone(), deadline).await;
        attach.stop_output();
        let (mut rx, ended) = attach.subscribe();
        // Without read loops, the output never ends.
        if ended || !attach.has_output() {
            return;
        }
        let end = async {
            while !matches!(
                rx.recv().await,
                Ok(Message::Done) | Err(broadcast::error::RecvError::Closed)
            ) {}
        };
        if time::timeout_at(deadline, end).await.is_err() {
            warn!("Read loops did not end in time, not waiting for them");
        }
    }

    pub fn from_child(child: Child) -> (Box<str>, Self) {
        static NEXT_SEQ: AtomicU64 = AtomicU64::new(0);
        (
            child.id,
            Self {
                seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
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

    /// Watch the process and return the receiver for its exit, which exists
    /// before the task starts, a receiver subscribed after the exit would
    /// miss it. `forget` runs once the exit got sent, or the task ended
    /// otherwise, like with a panic. A task which never ran, which tokio
    /// drops right in `spawn` while the runtime shuts down, does not run it,
    /// so the caller may hold a lock which `forget` takes.
    fn watch(&mut self, forget: impl FnOnce() + Send + 'static) -> Receiver<ExitChannelData> {
        let exit_paths = self.exit_paths.clone();
        let oom_exit_paths = self.oom_exit_paths.clone();
        let pid = self.pid();
        // Only one exit code will be written.
        let (exit_tx, exit_rx) = broadcast::channel(1);
        let timeout = self.timeout();
        let stop_token = self.token().clone();
        let exit_token = stop_token.clone();
        let cleanup_cmd_raw = self.cleanup_cmd.clone();
        let io = self.io.clone();
        let kind = self.kind;

        let task = task::spawn(
            async move {
                // Only created on the first poll, see above.
                let _forget = OnDrop(Some(forget));
                debug!("Running task");
                let mut exit_code = EXIT_CODE_UNKNOWN;
                let mut oomed = false;
                let mut timed_out = false;
                // Nobody learns about an OOM without the files, like an exec.
                let oom = async {
                    if oom_exit_paths.is_empty() {
                        return None;
                    }
                    OOMWatcher::watch(&stop_token, pid, &oom_exit_paths)
                        .instrument(debug_span!("cgroup_handling", %pid))
                        .await
                };
                let mut oom = pin!(oom);

                let wait_for_exit_code = Self::wait_for_exit_code_async(stop_token.clone(), pid)
                    .instrument(debug_span!("wait_for_exit_code"));

                // Known once the process got reaped and once the OOM watcher
                // ended, also if the timeout hits while waiting for the other.
                let mut reaped = None;
                let mut oom_result = None;
                let closure = async {
                    let wait_for_exit_code = async {
                        let code = wait_for_exit_code.await;
                        reaped = Some(code);
                        code
                    };
                    let watch_oom = async {
                        let event = oom.as_mut().await;
                        oom_result = Some(event);
                        event
                    };
                    let (code, oom) = tokio::join!(wait_for_exit_code, watch_oom);
                    exit_code = code;
                    oomed = Self::handle_oom(oom, code, &oom_exit_paths).await;
                };
                if let Some(timeout) = timeout {
                    let res = time::timeout_at(timeout, closure).await;
                    if let (Err(_), Some(code)) = (&res, reaped) {
                        // It exited in time, only the OOM check did not end.
                        debug!("Process exited before its timeout");
                        exit_code = code;
                        oomed = match oom_result {
                            Some(event) => Self::handle_oom(event, code, &oom_exit_paths).await,
                            None => Self::late_oom(oom, code, &oom_exit_paths).await,
                        };
                    } else if res.is_err() {
                        timed_out = true;
                        exit_code = EXIT_CODE_TIMED_OUT;
                        kill_grandchild(pid, Signal::SIGKILL);
                        exit_token.cancel();
                        // Reap it once the SIGKILL took effect, the wait got
                        // dropped with the timeout. A process stuck in the
                        // kernel keeps only a pidfd busy, not a thread.
                        task::spawn(Self::wait_for_exit_code_async(
                            CancellationToken::new(),
                            pid,
                        ));
                    }
                } else {
                    closure.await;
                }
                // Tells the users of the token that the process is gone.
                exit_token.cancel();

                // The exit file tells that the log is complete, so let the read
                // loops write what the process left in the pipes, and stop
                // them, see `stop_output`. The output of an exec gets stopped
                // by its consumer, see `StopOutput`.
                if kind == ChildKind::Container {
                    Self::finish_output(io.attach().await).await;
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
            .instrument(debug_span!("watch", %pid)),
        );

        self.task = Some(Arc::new(Mutex::new(Some(task))));
        exit_rx
    }

    fn spawn_cleanup_process(raw_cmd: &[String]) {
        let mut cleanup_cmd = Command::new(&raw_cmd[0]);
        init::reset_inherited(&mut cleanup_cmd);

        cleanup_cmd.args(&raw_cmd[1..]).stdin(Stdio::null());

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
    pub(crate) async fn wait_for_exit_code_async(token: CancellationToken, pid: Pid) -> i32 {
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
                    EXIT_CODE_WAIT_FAILED
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
            if let Some(f) = lock(slot).take() {
                f();
            }
        }

        let thread_slot = slot.clone();
        if let Err(e) = thread::Builder::new()
            .name("waitpid".into())
            .stack_size(Self::WAITPID_STACK_SIZE)
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
                Ok(None) => continue,
                Err(Errno::EINTR) => {
                    debug!("Failed to wait for pid on EINTR, retrying");
                    continue;
                }
                Err(err) => {
                    error!("Unable to waitpid on {err:#}");
                    return EXIT_CODE_WAIT_FAILED;
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
                    error!("Could not write exit file: {e:#}");
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
    use tokio::io::AsyncWriteExt;

    /// Spawn a child which exits right away with `code` and is never waited
    /// for.
    fn spawn_zombie(code: i32) -> Result<Pid> {
        let child = Command::new("sh")
            .args(["-c", &format!("exit {code}")])
            .spawn()?;
        let pid = child_pid(child.id())?;
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
        let (grace, max_defer) = (Duration::ZERO, Duration::from_secs(60));
        assert!(!ChildReaper::has_zombie());
        let orphan = spawn_zombie(0)?;
        let claimed = spawn_zombie(0)?;
        lock(&reaper.pending).insert(claimed);
        assert!(ChildReaper::has_zombie());

        // Both ways of listing the children find them.
        assert!(ChildReaper::zombie_children().0.contains(&orphan));
        assert!(children_by_scan().contains(&orphan));

        // The first sweep only records the unclaimed one as candidate.
        let mut candidates = HashMap::new();
        reaper
            .sweep_orphans(&mut candidates, Duration::from_secs(60), max_defer)
            .await;
        assert!(is_zombie(orphan));
        assert!(candidates.contains_key(&orphan));
        assert!(!candidates.contains_key(&claimed));

        // Without grace the unclaimed one is reaped, the claimed one stays.
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        assert!(!is_zombie(orphan));
        assert!(is_zombie(claimed));
        assert!(!candidates.contains_key(&orphan));

        // The process of a call in progress which exited before the call
        // claimed it, like an exec which ends before its runtime exited,
        // stays for the call, which gets its exit code.
        let call = reaper.begin_call(None)?;
        let exec = spawn_zombie(3)?;
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        assert!(is_zombie(exec));
        let token = CancellationToken::new();
        assert_eq!(
            ReapableChild::wait_for_exit_code_async(token.clone(), exec).await,
            3
        );
        drop(call);

        // A call which hangs keeps the sweep from reaping only up to the
        // bound.
        let call = reaper.begin_call(None)?;
        let late = spawn_zombie(0)?;
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        assert!(is_zombie(late));
        reaper
            .sweep_orphans(&mut candidates, grace, Duration::ZERO)
            .await;
        assert!(!is_zombie(late));
        drop(call);

        // A call which cannot claim a process anymore defers none, see
        // `deferral_ends_with_the_call_or_the_bound`.
        let past = Instant::now()
            .checked_sub(ChildReaper::CLAIM_MARGIN + Duration::from_secs(1))
            .context("past deadline")?;
        let call = reaper.begin_call(Some(past))?;
        let past_its_deadline = spawn_zombie(0)?;
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        assert!(!is_zombie(past_its_deadline));
        drop(call);

        // Calls which started after the zombie was first seen cannot claim
        // it, they do not defer it, neither after the calls it was seen with
        // ended nor without any.
        let call = reaper.begin_call(None)?;
        let first = spawn_zombie(0)?;
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        assert!(is_zombie(first));
        drop(call);
        let unrelated = reaper.begin_call(None)?;
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        assert!(!is_zombie(first));
        drop(unrelated);

        let second = spawn_zombie(0)?;
        reaper
            .sweep_orphans(&mut candidates, Duration::from_secs(60), max_defer)
            .await;
        assert!(candidates.contains_key(&second));
        let _in_progress = reaper.begin_call(None)?;
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        assert!(!is_zombie(second));

        // Once the server shuts down, the calls in progress cannot register
        // a process anymore and do not defer any, like the one above which
        // started before this zombie.
        let shutting_down = spawn_zombie(0)?;
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        assert!(is_zombie(shutting_down));
        reaper.shutting_down.store(true, Ordering::SeqCst);
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        assert!(!is_zombie(shutting_down));
        reaper.shutting_down.store(false, Ordering::SeqCst);

        waitpid(claimed, None)?;
        assert!(!ChildReaper::zombie_children().0.contains(&claimed));
        assert!(!ChildReaper::has_zombie());

        // Without zombies the sweep forgets its candidates.
        candidates.insert(
            claimed,
            Candidate {
                first_seen: Instant::now(),
                calls_before: 0,
            },
        );
        reaper
            .sweep_orphans(&mut candidates, grace, max_defer)
            .await;
        assert!(candidates.is_empty());
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

    fn is_gone(pid: Pid) -> bool {
        kill(pid, None).is_err()
    }

    /// A shell which nobody but the watcher waits for. It gets killed through
    /// a pidfd if a test fails before it ended, which cannot hit a reused PID.
    struct Spawned {
        pid: Pid,
        process: Process,
    }

    impl Drop for Spawned {
        fn drop(&mut self) {
            let _ = self.process.signal(Signal::SIGKILL);
        }
    }

    fn spawn_sh(script: &str) -> Result<Spawned> {
        let pid = child_pid(Command::new("sh").args(["-c", script]).spawn()?.id())?;
        // Not reaped yet, so the PID is still its own.
        let process = Process::open(pid);
        Ok(Spawned { pid, process })
    }

    fn child(
        pid: Pid,
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
            cleanup_cmd: cleanup.iter().map(ToString::to_string).collect(),
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
            let mut exit_rx = reapable.watch(|| {});

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
        // The PID of the first one got reused.
        let (first, second, third) = (
            reapable(Pid::from_raw(1))?,
            reapable(Pid::from_raw(1))?,
            reapable(Pid::from_raw(3))?,
        );
        let (first_seq, third_seq) = (first.seq, third.seq);
        let second_seq = second.seq;
        let map = Arc::new(Mutex::new(HashMap::from([
            ("a".into(), vec![first, second]),
            ("b".into(), vec![third]),
        ])));

        ChildReaper::forget_grandchild(&map, "b", third_seq);
        ChildReaper::forget_grandchild(&map, "a", first_seq);
        // Unknown registrations are ignored.
        ChildReaper::forget_grandchild(&map, "a", first_seq);
        ChildReaper::forget_grandchild(&map, "c", first_seq);

        let map = lock(&map);
        assert_eq!(map.len(), 1);
        assert_eq!(
            map["a"].iter().map(|c| c.seq).collect::<Vec<_>>(),
            [second_seq]
        );
        Ok(())
    }

    /// Clients match the start of the error, see `ErrContainerNotFound` of
    /// the Go client.
    #[test]
    fn get_unknown_container_fails_with_stable_error() {
        let err = ChildReaper::default()
            .get("unknown")
            .err()
            .map(|e| format!("{e:#}"));
        assert_eq!(err.as_deref(), Some("container not found: unknown"));
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
        let mut exit_rx = reapable.watch(|| {});

        let exit = exit_rx.recv().await?;
        assert_eq!(exit.exit_code, EXIT_CODE_TIMED_OUT);
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
        lock(&reaper.pending).insert(pid);
        reaper.cleanup_failed_grandchild(&pidfile);
        assert!(!is_gone(pid) && !is_zombie(pid));

        lock(&reaper.pending).remove(&pid);
        reaper.cleanup_failed_grandchild(&pidfile);
        wait_until(|| is_gone(pid)).await;
        Ok(())
    }

    #[tokio::test]
    async fn registration_keeps_the_child_claimed() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let sh = spawn_sh("exec sleep 60")?;
        // As `create_child` leaves it.
        let mut call = reaper.begin_call(None)?;
        call.pending = Some(PendingPid::new(&reaper.pending, sh.pid));
        assert!(reaper.is_claimed(sh.pid));

        let child = child(sh.pid, None, &dir.path().join("exit"), &[])?;
        reaper.watch_grandchild(child, vec![], call)?;
        assert!(reaper.is_claimed(sh.pid));
        assert!(!lock(&reaper.pending).contains(&sh.pid));
        assert!(!reaper.calls_in_progress());
        Ok(())
    }

    #[tokio::test]
    async fn leaked_fds_stay_open_until_the_exit() -> Result<()> {
        use nix::{fcntl::OFlag, unistd::pipe2};
        use std::io::{ErrorKind, Read};

        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let sh = spawn_sh("exec sleep 60")?;
        let (reader, writer) = pipe2(OFlag::O_NONBLOCK)?;
        let mut reader = std::fs::File::from(reader);
        let call = reaper.begin_call(None)?;
        let child = child(sh.pid, None, &dir.path().join("exit"), &[])?;
        let mut exit_rx = reaper.watch_grandchild(child, vec![writer], call)?;

        // The server keeps the descriptor while the process runs.
        let mut buf = [0; 1];
        assert_eq!(
            reader.read(&mut buf).map_err(|e| e.kind()),
            Err(ErrorKind::WouldBlock)
        );

        sh.process.signal(Signal::SIGKILL)?;
        exit_rx.recv().await?;
        wait_until(|| matches!((&reader).read(&mut [0; 1]), Ok(0))).await;
        assert!(matches!(reader.read(&mut buf), Ok(0)));
        Ok(())
    }

    #[tokio::test]
    async fn abandoned_call_releases_its_process() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let pidfile = dir.path().join("pid");
        let script = format!("sleep 60 & echo $! > {}", pidfile.display());
        let args = ["-c".into(), script];
        let mut io = ContainerIO::new(false, ContainerLog::new())?;
        let (pid, token, call) = reaper
            .create_child(RuntimeCall::new(Path::new("sh"), &args, &pidfile), &mut io)
            .await?;
        // Not a child of the tests, which are no subreaper.
        let process = Process::open(pid);
        token.cancel();
        assert!(reaper.is_claimed(pid));
        assert!(reaper.calls_in_progress());

        // A caller which gives up before the registration, or whose future
        // gets dropped, does not leave the process claimed.
        drop(call);
        assert!(!reaper.is_claimed(pid));
        assert!(!reaper.calls_in_progress());
        process.signal(Signal::SIGKILL)?;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_without_stdin_gets_dev_null() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let (pidfile, stdin) = (dir.path().join("pid"), dir.path().join("stdin"));
        let script = format!(
            "readlink /proc/self/fd/0 > {}; echo $$ > {}",
            stdin.display(),
            pidfile.display()
        );
        let args = ["-c".into(), script];
        let mut io = ContainerIO::new(false, ContainerLog::new())?;
        let (_, token, _call) = reaper
            .create_child(RuntimeCall::new(Path::new("sh"), &args, &pidfile), &mut io)
            .await?;
        token.cancel();
        assert_eq!(std::fs::read_to_string(stdin)?.trim(), "/dev/null");
        Ok(())
    }

    #[tokio::test]
    async fn registration_after_shutdown_kills_child() -> Result<()> {
        let reaper = ChildReaper::default();
        reaper
            .kill_grandchildren(Instant::now() + Duration::from_secs(10))
            .await?;

        // A call which was in progress while the shutdown killed the others.
        let dir = tempfile::tempdir()?;
        let sh = spawn_sh("exec sleep 60")?;
        let pid = sh.pid;
        assert!(
            reaper
                .watch_grandchild(
                    child(pid, None, &dir.path().join("exit"), &[])?,
                    vec![],
                    InFlight::default()
                )
                .is_err()
        );
        wait_until(|| is_gone(pid)).await;

        // New calls do not start at all.
        let mut io = ContainerIO::new(false, ContainerLog::new())?;
        let pidfile = dir.path().join("pid");
        let args = ["x".into()];
        let res = reaper
            .create_child(
                RuntimeCall::new(Path::new("true"), &args, &pidfile),
                &mut io,
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
        let confirming = async {
            time::sleep(Duration::from_secs(1)).await;
            Some(OOMEvent::possible(true))
        };
        assert!(ReapableChild::late_oom(confirming, 137, &paths).await);
        assert!(oom_path.exists());

        // A normal exit is none, and no result at all either.
        std::fs::remove_file(&oom_path)?;
        let possible = async { Some(OOMEvent::possible(true)) };
        assert!(!ReapableChild::late_oom(possible, 0, &paths).await);
        let start = Instant::now();
        assert!(!ReapableChild::late_oom(std::future::pending(), 137, &paths).await);
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
        let pidfile = dir.path().join("pid");
        let args = ["-c".into(), script.into()];
        let call = RuntimeCall {
            deadline: Some(Instant::now() + Duration::from_secs(10)),
            ..RuntimeCall::new(Path::new("sh"), &args, &pidfile)
        };
        let res =
            time::timeout(Duration::from_secs(20), reaper.create_child(call, &mut io)).await?;
        let err = format!("{:#}", res.err().context("runtime failure expected")?);
        assert!(err.contains("exited with: 1"), "{err}");
        assert!(err.trim_end().ends_with("the-error"));
        assert!(err.len() < 2 * RuntimeOutput::MAX_STDERR);
        assert!(start.elapsed() < Duration::from_secs(10));
        Ok(())
    }

    #[tokio::test]
    async fn hanging_runtime_gets_killed_at_its_deadline() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let pidfile = dir.path().join("pid");
        let args = ["-c".into(), "exec sleep 60".into()];
        for terminal in [false, true] {
            let mut io = ContainerIO::new(terminal, ContainerLog::new())?;
            let call = RuntimeCall {
                deadline: Some(Instant::now() + Duration::from_millis(200)),
                ..RuntimeCall::new(Path::new("sh"), &args, &pidfile)
            };
            let start = Instant::now();
            let res =
                time::timeout(Duration::from_secs(20), reaper.create_child(call, &mut io)).await?;
            let err = format!("{:#}", res.err().context("runtime failure expected")?);
            assert!(err.contains("did not finish in time"), "{err}");
            assert!(start.elapsed() < Duration::from_secs(10));
            // The call ended with it.
            assert!(!reaper.calls_in_progress());
        }
        Ok(())
    }

    #[tokio::test]
    async fn hanging_runtime_gets_killed_with_its_group() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let run = async |terminal: bool| -> Result<()> {
            let pidfile = dir.path().join(format!("pid-{terminal}"));
            let helper_pidfile = dir.path().join(format!("helper-{terminal}"));
            // Like a runc init which the runtime started before it hung,
            // which holds the pipes of the runtime as well.
            let script = format!(
                "sleep 60 & echo $! > {}; exec sleep 60",
                helper_pidfile.display()
            );
            let args = ["-c".into(), script];
            let mut io = ContainerIO::new(terminal, ContainerLog::new())?;
            let call = RuntimeCall {
                deadline: Some(Instant::now() + Duration::from_secs(1)),
                ..RuntimeCall::new(Path::new("sh"), &args, &pidfile)
            };
            // Its output does not wait for the helper either, which runs
            // for a minute.
            let res =
                time::timeout(Duration::from_secs(20), reaper.create_child(call, &mut io)).await?;
            let err = format!("{:#}", res.err().context("runtime failure expected")?);
            assert!(err.contains("did not finish in time"), "{err}");
            // Reparented to the init or a subreaper above the tests.
            let helper = read_pid_file(&helper_pidfile)?;
            wait_until(|| is_gone(helper) || is_zombie(helper)).await;
            Ok(())
        };
        let (streams, terminal) = tokio::join!(run(false), run(true));
        streams.and(terminal)
    }

    #[tokio::test]
    async fn stale_pid_file_does_not_get_cleaned_up() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let pidfile = dir.path().join("pid");
        // A child of the server which nobody registered, like the pause init.
        let other = spawn_sh("exec sleep 60")?;
        std::fs::write(&pidfile, other.pid.to_string())?;

        let mut io = ContainerIO::new(false, ContainerLog::new())?;
        let args = ["-c".into(), "exit 1".into()];
        let call = RuntimeCall::new(Path::new("sh"), &args, &pidfile);
        assert!(reaper.create_child(call, &mut io).await.is_err());
        assert!(!pidfile.exists());
        time::sleep(Duration::from_millis(100)).await;
        assert!(!is_gone(other.pid) && !is_zombie(other.pid));
        Ok(())
    }

    #[tokio::test]
    async fn dropped_runtime_gets_killed_with_its_group() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let pidfile = dir.path().join("pid");
        let helper_pidfile = dir.path().join("helper");
        let script = format!(
            "sleep 60 & echo $! > {}; exec sleep 60",
            helper_pidfile.display()
        );
        let args = ["-c".into(), script];
        let mut io = ContainerIO::new(false, ContainerLog::new())?;
        let call = RuntimeCall::new(Path::new("sh"), &args, &pidfile);
        // Like a call in progress which the shutdown drops.
        select! {
            _ = reaper.create_child(call, &mut io) => bail!("runtime finished"),
            () = wait_until(|| read_pid_file(&helper_pidfile).is_ok()) => {}
        }
        let helper = read_pid_file(&helper_pidfile)?;
        wait_until(|| is_gone(helper) || is_zombie(helper)).await;
        Ok(())
    }

    #[tokio::test]
    async fn wait_for_reaped_process_is_unknown() -> Result<()> {
        let mut reaped = Command::new("true").spawn()?;
        let pid = child_pid(reaped.id())?;
        reaped.wait()?;
        let token = CancellationToken::new();
        let code = time::timeout(
            Duration::from_secs(5),
            ReapableChild::wait_for_exit_code_async(token.clone(), pid),
        )
        .await?;
        assert_eq!(code, EXIT_CODE_UNKNOWN);
        assert!(token.is_cancelled());
        Ok(())
    }

    #[test]
    fn late_grandchildren_which_are_gone_are_not_signalled() -> Result<()> {
        let mut reaped = Command::new("true").spawn()?;
        let pid = child_pid(reaped.id())?;
        reaped.wait()?;
        assert!(!ChildReaper::kill_late_grandchild(pid));

        let sh = spawn_sh("exec sleep 60")?;
        assert!(ChildReaper::kill_late_grandchild(sh.pid));
        assert!(matches!(
            waitpid(sh.pid, None)?,
            WaitStatus::Signaled(_, Signal::SIGKILL, _)
        ));
        Ok(())
    }

    /// The exit of an exec whose runtime is slow, but within its deadline,
    /// stays for the call, a hung create does not defer orphans for longer
    /// than the bound.
    #[tokio::test(start_paused = true)]
    async fn deferral_ends_with_the_call_or_the_bound() -> Result<()> {
        let reaper = ChildReaper::default();
        let max_defer = ChildReaper::ORPHAN_MAX_DEFER;
        let first_seen = |reaper: &ChildReaper| Candidate {
            first_seen: Instant::now(),
            calls_before: reaper.in_flight.borrow().next,
        };

        // The runtime of an exec gets a minute, its process exits after
        // 29.5s and the runtime after 31s.
        let exec = reaper.begin_call(Some(Instant::now() + Duration::from_secs(60)))?;
        time::advance(Duration::from_millis(29_500)).await;
        let zombie = first_seen(&reaper);
        time::advance(Duration::from_millis(1500)).await;
        assert!(reaper.defers(zombie, Instant::now(), max_defer));
        // Not past the bound since it was first seen.
        time::advance(max_defer - Duration::from_secs(2)).await;
        assert!(reaper.defers(zombie, Instant::now(), max_defer));
        time::advance(Duration::from_secs(2)).await;
        assert!(!reaper.defers(zombie, Instant::now(), max_defer));

        // Not past the deadline of the call and its margin either.
        let late = first_seen(&reaper);
        time::advance(ChildReaper::CLAIM_MARGIN - Duration::from_millis(1500)).await;
        assert!(reaper.defers(late, Instant::now(), max_defer));
        time::advance(Duration::from_secs(1)).await;
        assert!(!reaper.defers(late, Instant::now(), max_defer));
        drop(exec);

        // A create gets 10 minutes, the bound since the zombie was first
        // seen ends the deferral first, also late in the call.
        let create = reaper.begin_call(Some(Instant::now() + Duration::from_secs(600)))?;
        for after in [Duration::from_secs(1), Duration::from_secs(500)] {
            time::advance(after).await;
            let zombie = first_seen(&reaper);
            time::advance(max_defer - Duration::from_millis(1)).await;
            assert!(reaper.defers(zombie, Instant::now(), max_defer));
            time::advance(Duration::from_millis(1)).await;
            assert!(!reaper.defers(zombie, Instant::now(), max_defer));
        }

        // An older call with a later deadline still claims, a call which
        // started after the zombie was first seen does not.
        let exec = reaper.begin_call(Some(Instant::now() + Duration::from_secs(1)))?;
        let zombie = first_seen(&reaper);
        let unrelated = reaper.begin_call(Some(Instant::now() + Duration::from_secs(60)))?;
        time::advance(Duration::from_secs(20)).await;
        assert!(reaper.defers(zombie, Instant::now(), max_defer));
        drop(create);
        assert!(!reaper.defers(zombie, Instant::now(), max_defer));
        drop((exec, unrelated));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_calls_in_progress() -> Result<()> {
        let reaper = Arc::new(ChildReaper::default());
        let call = reaper.begin_call(None)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let shutdown = task::spawn({
            let reaper = reaper.clone();
            async move { reaper.kill_grandchildren(deadline).await }
        });
        time::sleep(Duration::from_secs(1)).await;
        assert!(!shutdown.is_finished());
        // No new call starts meanwhile.
        assert!(reaper.begin_call(None).is_err());

        drop(call);
        shutdown.await??;

        // A call which never ends does not keep the shutdown forever, and
        // the shutdown reports it.
        for (deadline, waited) in [
            (Duration::from_secs(60), ChildReaper::IN_FLIGHT_WAIT),
            (Duration::from_secs(2), Duration::from_secs(2)),
        ] {
            let reaper = ChildReaper::default();
            let _call = reaper.begin_call(None)?;
            let start = Instant::now();
            let err = reaper
                .kill_grandchildren(start + deadline)
                .await
                .err()
                .map(|e| format!("{e:#}"));
            assert_eq!(err.as_deref(), Some("runtime calls still in progress"));
            assert_eq!(start.elapsed(), waited);
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_does_not_wait_forever_for_a_watcher() -> Result<()> {
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let sh = spawn_sh("exec sleep 60")?;
        let (id, mut stuck) =
            ReapableChild::from_child(child(sh.pid, None, &dir.path().join("exit"), &[])?);
        // Like the watcher of a process stuck in the kernel.
        stuck.task = Some(Arc::new(Mutex::new(Some(task::spawn(
            std::future::pending(),
        )))));
        lock(&reaper.grandchildren).insert(id, vec![stuck]);

        let start = Instant::now();
        let err = reaper
            .kill_grandchildren(start + Duration::from_secs(8))
            .await
            .err()
            .map(|e| format!("{e:#}"));
        assert_eq!(err, Some(format!("PIDs [{}] did not exit", sh.pid)));
        assert_eq!(start.elapsed(), Duration::from_secs(8));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn exit_files_wait_for_quiet_output() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let output = attach.output();

        // Nothing flows, a process holds the pipes open.
        let start = Instant::now();
        ReapableChild::wait_for_output(attach.clone(), Instant::now() + DRAIN_MAX_LOG_WAIT).await;
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
        ReapableChild::wait_for_output(attach.clone(), Instant::now() + DRAIN_MAX_LOG_WAIT).await;
        assert_eq!(start.elapsed(), DRAIN_MAX_WAIT);
        writer.abort();

        // Ended output does not wait at all.
        drop(output);
        let start = Instant::now();
        ReapableChild::wait_for_output(attach, Instant::now() + DRAIN_MAX_LOG_WAIT).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        Ok(())
    }

    /// A read loop over a pipe, whose log drivers hang while a guard from
    /// [`hold`] holds the log.
    struct LogLoop {
        writer: tokio::io::DuplexStream,
        attach: SharedContainerAttach,
        logger: crate::container_log::SharedContainerLog,
        cri: tempfile::NamedTempFile,
        json: tempfile::NamedTempFile,
        task: JoinHandle<Result<()>>,
    }

    async fn log_loop() -> Result<LogLoop> {
        let (cri, json, logger) = crate::container_log::test_helpers::container_log(true).await?;
        let attach = SharedContainerAttach::default();
        let (writer, reader) = tokio::io::duplex(1024);
        let task = task::spawn(ContainerIO::read_loop(
            reader,
            crate::container_io::Pipe::StdOut,
            logger.clone(),
            None,
            attach.clone(),
            attach.output(),
        ));
        Ok(LogLoop {
            writer,
            attach,
            logger,
            cri,
            json,
            task,
        })
    }

    /// Make the log drivers hang until the guard gets dropped.
    async fn hold(
        logger: &crate::container_log::SharedContainerLog,
    ) -> tokio::sync::OwnedRwLockReadGuard<ContainerLog> {
        logger.clone().read_owned().await
    }

    /// Wait until a read loop has output on its way into the log, failing
    /// if it never does.
    async fn wait_logging(attach: &SharedContainerAttach) -> Result<()> {
        for _ in 0..1000 {
            if attach.is_logging() {
                return Ok(());
            }
            task::yield_now().await;
        }
        bail!("no output on its way into the log")
    }

    #[tokio::test(start_paused = true)]
    async fn exit_files_wait_for_slow_log_drivers() -> Result<()> {
        let mut l = log_loop().await?;
        let held = hold(&l.logger).await;
        l.writer.write_all(b"last line\n").await?;
        wait_logging(&l.attach).await?;

        // The log takes longer than the drain waits for a pause.
        let wait = task::spawn(ReapableChild::wait_for_output(
            l.attach.clone(),
            Instant::now() + DRAIN_MAX_LOG_WAIT,
        ));
        time::sleep(2 * DRAIN_MAX_WAIT).await;
        assert!(!wait.is_finished());

        drop(held);
        drop(l.writer);
        wait.await?;
        assert!(crate::container_log::test_helpers::read(&l.cri)?.contains("last line"));
        l.task.await?
    }

    #[tokio::test(start_paused = true)]
    async fn exit_files_wait_for_the_last_line() -> Result<()> {
        let mut l = log_loop().await?;
        let (mut rx, _) = l.attach.subscribe();
        l.writer.write_all(b"no newline").await?;
        // Broadcast once the chunk got logged, the line still waits.
        rx.recv().await?;

        // The output ended, its last line is on its way into the log.
        let held = hold(&l.logger).await;
        drop(l.writer);
        wait_logging(&l.attach).await?;
        let wait = task::spawn(ReapableChild::wait_for_output(
            l.attach.clone(),
            Instant::now() + DRAIN_MAX_LOG_WAIT,
        ));
        time::sleep(2 * DRAIN_MAX_WAIT).await;
        assert!(!wait.is_finished());

        drop(held);
        wait.await?;
        assert!(
            crate::container_log::test_helpers::read(&l.json)?
                .contains(r#""message":"no newline","partial":true"#)
        );
        l.task.await?
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_gets_logged_after_the_output_finished() -> Result<()> {
        let mut l = log_loop().await?;
        let (mut rx, _) = l.attach.subscribe();
        l.writer.write_all(b"no newline").await?;
        rx.recv().await?;

        // A process left behind keeps the pipe open, the line waits for its
        // ending. It gets written once the read loop ended.
        ReapableChild::finish_output(l.attach.clone()).await;
        (&mut l.task).await??;
        let read = || crate::container_log::test_helpers::read(&l.json);
        assert!(read()?.contains(r#""message":"no newline","partial":true"#));

        // Its later output does not reach the log anymore.
        assert!(l.writer.write_all(b"late\n").await.is_err());
        assert!(!read()?.contains("late"));
        Ok(())
    }

    /// The output of an exec whose consumer is slower than the drain after
    /// the exit waits for a pause, which blocks the read loop, arrives in
    /// full.
    #[tokio::test(start_paused = true)]
    async fn slow_exec_consumers_get_all_output() -> Result<()> {
        use crate::container_io::{STDIO_CHANNEL_SIZE, StdioSender};

        const OUTPUT: usize = 1024 * 1024;
        let reaper = ChildReaper::default();
        let dir = tempfile::tempdir()?;
        let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
        let attach = io.attach().await;
        let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        let sender = StdioSender::new(tx, &Arc::new(AtomicBool::new(true)));
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        task::spawn(ContainerIO::read_loop(
            reader,
            crate::container_io::Pipe::StdOut,
            ContainerLog::new(),
            Some(sender),
            attach.clone(),
            attach.output(),
        ));
        let write = task::spawn(async move {
            writer.write_all(&vec![b'x'; OUTPUT]).await?;
            Ok::<_, std::io::Error>(writer)
        });

        // The process wrote its output and exited.
        let sh = spawn_sh("exit 0")?;
        let call = reaper.begin_call(None)?;
        let mut child = child(sh.pid, None, &dir.path().join("exit"), &[])?;
        child.kind = ChildKind::Exec;
        child.exit_paths = vec![];
        child.io = io.clone();
        let token = child.token.clone();
        let _stop = attach.stop_output_on_exit(token.clone());
        let mut exit_rx = reaper.watch_grandchild(child, vec![], call)?;
        exit_rx.recv().await?;

        let mut received = 0;
        while received < OUTPUT {
            time::sleep(4 * DRAIN_TIMEOUT).await;
            match rx.recv().await? {
                Message::Data(data, _) => received += data.len(),
                Message::Done => break,
            }
        }
        assert_eq!(received, OUTPUT);
        drop(write.await??);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn finished_output_is_bounded_with_a_hanging_log() -> Result<()> {
        let mut l = log_loop().await?;
        let held = hold(&l.logger).await;
        l.writer.write_all(b"never logged\n").await?;
        wait_logging(&l.attach).await?;

        let start = Instant::now();
        ReapableChild::finish_output(l.attach.clone()).await;
        assert_eq!(start.elapsed(), DRAIN_MAX_LOG_WAIT);

        drop(held);
        drop(l.writer);
        l.task.await?
    }

    #[tokio::test(start_paused = true)]
    async fn exit_files_wait_for_log_drivers_bounded() -> Result<()> {
        let mut l = log_loop().await?;
        let held = hold(&l.logger).await;
        l.writer.write_all(b"never logged\n").await?;
        wait_logging(&l.attach).await?;

        let start = Instant::now();
        ReapableChild::wait_for_output(l.attach, Instant::now() + DRAIN_MAX_LOG_WAIT).await;
        assert_eq!(start.elapsed(), DRAIN_MAX_LOG_WAIT);

        drop(held);
        drop(l.writer);
        l.task.await?
    }
}
