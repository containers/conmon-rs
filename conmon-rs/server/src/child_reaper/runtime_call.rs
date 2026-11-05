//! Running a runtime for a call: its output, its process group and the
//! cleanup after a failure.

use super::{ChildReaper, InFlight, PendingPid};
use crate::{
    container_io::{ContainerIO, READ_BUFFER_SIZE, with_scratch},
    fs_util, init,
    mutex::lock,
    process::{Process, child_pid, read_pid_file, signal_name, wait_for_exit_code},
    runtime_args::LoggedArgs,
    tail::Tail,
    util::with_stderr,
};
use anyhow::{Context, Result, format_err};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::{
    future, io,
    ops::{Deref, DerefMut},
    os::unix::process::ExitStatusExt,
    path::Path,
    pin::{Pin, pin},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    task::{Poll, ready},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    process::{Child as RuntimeChild, ChildStderr, ChildStdout, Command},
    select,
    task::{self, JoinHandle},
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

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
    pub(super) const MAX_STDERR: usize = 64 * 1024;

    /// Start reading the pipes of a runtime in the background.
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

    /// Read until the end, keeping the end in `buf` if any. The reads go to
    /// the scratch buffer of the thread, see [`with_scratch`], so that the
    /// task of a create or exec with a terminal carries no buffers of its
    /// own.
    async fn read(mut reader: impl AsyncRead + Unpin, buf: Option<&Mutex<StderrTail>>) {
        let mut read = |cx: &mut std::task::Context<'_>| {
            with_scratch(READ_BUFFER_SIZE, |scratch| {
                let mut chunk = ReadBuf::new(scratch);
                match ready!(Pin::new(&mut reader).poll_read(cx, &mut chunk)) {
                    Ok(()) if !chunk.filled().is_empty() => {
                        if let Some(buf) = buf {
                            lock(buf).push(chunk.filled());
                        }
                        Poll::Ready(true)
                    }
                    Ok(()) | Err(_) => Poll::Ready(false),
                }
            })
        };
        while future::poll_fn(&mut read).await {}
    }

    /// Wait for the end of the output for a while, a helper of the runtime
    /// may hold the pipes open, but only briefly once the shutdown stopped
    /// the call through `stop`, and return the end of stderr.
    async fn finish_unless_stopped(output: Option<Self>, stop: &CancellationToken) -> Vec<u8> {
        let Some(mut output) = output else {
            return vec![];
        };
        let task = &mut output.task;
        let grace = ChildReaper::RUNTIME_GRACE;
        if wait_unless_stopped(task, grace, stop, ChildReaper::STOPPED_OUTPUT_WAIT)
            .await
            .is_none()
        {
            debug!("Runtime output still held open, giving up on it");
            output.task.abort();
        }
        Self::end(&output)
    }

    fn end(output: &Self) -> Vec<u8> {
        std::mem::take(&mut *lock(&output.stderr)).into_end()
    }
}

/// How [`ChildReaper::create_child`] runs a runtime.
#[derive(Debug)]
pub(crate) struct RuntimeCall<'a> {
    /// The binary of the runtime.
    pub(crate) runtime: &'a Path,
    pub(crate) args: &'a [String],

    /// Where the command of an exec starts in `args`, which does not get
    /// logged, see [`LoggedArgs`].
    pub(crate) command_start: Option<usize>,

    /// Whether the process gets a stdin pipe.
    pub(crate) stdin: bool,

    /// Where the runtime writes the PID of the process it started.
    pub(crate) pidfile: &'a Path,

    /// When a runtime which did not finish gets killed.
    pub(crate) deadline: Instant,
}

impl<'a> RuntimeCall<'a> {
    /// A call without stdin, whose runtime gets killed at the `deadline`.
    pub(crate) fn new(
        runtime: &'a Path,
        args: &'a [String],
        pidfile: &'a Path,
        deadline: Instant,
    ) -> Self {
        Self {
            runtime,
            args,
            command_start: None,
            stdin: false,
            pidfile,
            deadline,
        }
    }
}

impl ChildReaper {
    /// Spawn the runtime, see [`ChildReaper::create_child`].
    fn spawn_runtime(runtime: &Path, args: &[String], stdin: bool) -> Result<RuntimeChild> {
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

        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        // Lowers the open files limit to the one the server started with.
        init::reset_inherited(&mut cmd);
        Ok(cmd.spawn()?)
    }

    /// How long a runtime gets for what follows its exit or its failure: one
    /// which exited successfully to connect the terminal, one which failed
    /// to exit on its own and one which got killed to exit. The helpers of a
    /// runtime which exited get it to close its output, see
    /// [`RuntimeOutput::finish_unless_stopped`].
    pub(crate) const RUNTIME_GRACE: Duration = Duration::from_secs(5);

    /// How long a call which the shutdown stopped waits for the exit of its
    /// killed runtime, and then for the output of the runtime, instead of
    /// the [`ChildReaper::RUNTIME_GRACE`], see [`ChildReaper::STOP_BUDGET`].
    pub(super) const STOPPED_EXIT_WAIT: Duration = Duration::from_millis(500);
    pub(super) const STOPPED_OUTPUT_WAIT: Duration = Duration::from_millis(250);

    /// Run the runtime with the I/O of the process it starts, and return the
    /// PID of the process once the runtime exited. The caller registers it
    /// through [`ChildReaper::watch_grandchild`].
    pub(crate) async fn create_child(
        &self,
        call: RuntimeCall<'_>,
        container_io: &mut ContainerIO,
    ) -> Result<(Pid, CancellationToken, InFlight)> {
        let RuntimeCall {
            runtime,
            args,
            command_start,
            stdin,
            pidfile,
            deadline: runtime_deadline,
        } = call;
        let mut in_flight = self.begin_call(runtime_deadline)?;
        // A PID file left from before, like of a container created in the
        // same bundle earlier, would name a process which the cleanup of a
        // failed call kills, see `cleanup_failed_grandchild`.
        fs_util::remove_file(pidfile).context("remove stale PID file")?;
        debug!(
            "Running: {:?} {:?}",
            runtime,
            LoggedArgs {
                args,
                command_start
            }
        );
        let child = Self::spawn_runtime(runtime, args, stdin).context("spawn child process")?;

        // Not waited for yet, so it has its PID. Dropping it on a failure
        // kills it.
        let runtime_pid = child_pid(child.id()).context("PID of runtime")?;
        let mut child = Runtime {
            child,
            pending: Some(PendingPid::new(&self.pending, runtime_pid)),
        };

        debug!("Running child on PID: {runtime_pid}");

        let token = CancellationToken::new();
        let stop = in_flight.stop.clone();

        // With streams, the read loops take the output of the runtime along
        // with the one of the process. With a terminal, the runtime has pipes
        // of its own. They get read while waiting for it, a runtime which
        // writes more than they hold, like with debug logging, would block.
        let output = container_io
            .terminal()
            .is_some()
            .then(|| RuntimeOutput::spawn(child.stdout.take(), child.stderr.take()));

        let connected = Self::connect_io(
            &mut child,
            container_io,
            stdin,
            &token,
            runtime_deadline,
            &stop,
        )
        .await;
        let status = match connected {
            Ok(()) => Self::wait_runtime(&mut child, runtime_deadline, &stop).await,
            Err(e) => Err(e),
        };
        let status = match status {
            Ok(status) => status,
            Err(RuntimeFailure { error, grace }) => {
                token.cancel();
                let stderr = Self::stop_runtime(child, output, grace, &stop).await;
                return Err(self.fail_create(pidfile, error, &stderr));
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
            in_flight.claim(&self.pending, *pid);
        }
        let stderr = RuntimeOutput::finish_unless_stopped(output, &stop).await;

        let error = match grandchild_pid {
            Some(Ok(pid)) => return Ok((pid, token, in_flight)),
            Some(Err(e)) => e,
            None => match status.code() {
                Some(code) => format_err!("runtime exited with status {code}"),
                // Without an exit code, a signal ended the runtime.
                None => {
                    let signal = status.signal().unwrap_or_default();
                    format_err!("runtime got killed by signal {}", signal_name(signal))
                }
            },
        };
        // Nothing registers the process, so the stdin loops have to be
        // stopped here. The caller stops the output once it took the error of
        // the runtime, see `ContainerIO::runtime_error`.
        token.cancel();
        Err(self.fail_create(pidfile, error, &stderr))
    }

    /// Connect the I/O of the process which the runtime starts: wait until
    /// the runtime connected the terminal, or read the pipes of the runtime,
    /// which the process shares. Fails once the `deadline` passed or the
    /// shutdown cancelled `stop`, like [`ChildReaper::wait_runtime`].
    async fn connect_io(
        child: &mut Runtime,
        container_io: &mut ContainerIO,
        stdin: bool,
        token: &CancellationToken,
        deadline: Instant,
        stop: &CancellationToken,
    ) -> Result<(), RuntimeFailure> {
        if container_io.terminal().is_none() {
            let (stdin, stdout, stderr) =
                (child.stdin.take(), child.stdout.take(), child.stderr.take());
            container_io.connect_streams(stdin, stdout, stderr, token.clone());
            return Ok(());
        }
        // The runtime connects to the console socket before it exits. If it
        // fails before that, waiting for the connection alone would hang
        // this call forever.
        select! {
            res = container_io.connect_terminal(stdin, token.clone()) => {
                res.context("wait for terminal socket connection")
                    .map_err(RuntimeFailure::new)
            }
            () = time::sleep_until(deadline) => Err(RuntimeFailure::timed_out()),
            () = stop.cancelled() => Err(RuntimeFailure::stopped()),
            status = child.reap() => match status.context("wait for child") {
                // The runtime succeeded, so it did connect and the descriptor
                // is queued already.
                Ok(status) if status.success() => {
                    let connect = container_io.connect_terminal(stdin, token.clone());
                    match wait_unless_stopped(connect, Self::RUNTIME_GRACE, stop, Duration::ZERO)
                        .await
                    {
                        Some(res) => res
                            .context("wait for terminal socket connection")
                            .map_err(RuntimeFailure::new),
                        None if stop.is_cancelled() => Err(RuntimeFailure::stopped()),
                        None => Err(RuntimeFailure::new(format_err!(
                            "runtime exited without connecting the terminal"
                        ))),
                    }
                }
                // A failure is reported later together with its output.
                Ok(_) => Ok(()),
                Err(e) => Err(RuntimeFailure::new(e)),
            },
        }
    }

    /// Wait for the exit of the runtime. A runtime which hangs gets killed
    /// once its deadline passed, and a wait which fails gets cleaned up like
    /// a runtime which failed. A runtime which still runs once the shutdown
    /// stops the call through `stop` gets killed right away as well, by
    /// this call, which holds it until it got reaped, see
    /// [`ChildReaper::kill_pending`].
    async fn wait_runtime(
        child: &mut Runtime,
        deadline: Instant,
        stop: &CancellationToken,
    ) -> Result<ExitStatus, RuntimeFailure> {
        select! {
            // A runtime which exited in time succeeded.
            biased;
            status = child.reap() => status
                .context("wait for runtime")
                .map_err(RuntimeFailure::new),
            () = time::sleep_until(deadline) => Err(RuntimeFailure::timed_out()),
            () = stop.cancelled() => Err(RuntimeFailure::stopped()),
        }
    }

    /// Stop a runtime which failed before it exited, or whose call the
    /// shutdown stopped: give it `grace` to exit on its own, so that it
    /// still writes the PID file the cleanup needs, kill it otherwise, and
    /// return its stderr. It gets killed through `child`, which tokio did
    /// not reap yet, so neither its PID nor its group got reused. Its
    /// pending mark ends once it got reaped, or once a killed runtime did
    /// not exit within [`ChildReaper::RUNTIME_GRACE`], before waiting for
    /// its output. Once the shutdown stopped the call through `stop`, the
    /// runtime gets killed right away and the waits get shorter, so that
    /// the call ends within the [`ChildReaper::STOP_BUDGET`].
    async fn stop_runtime(
        mut child: Runtime,
        output: Option<RuntimeOutput>,
        grace: Duration,
        stop: &CancellationToken,
    ) -> Vec<u8> {
        let exited = wait_unless_stopped(child.reap(), grace, stop, Duration::ZERO).await;
        if exited.is_none() {
            debug!("Runtime did not exit in time, killing it");
            // Its helpers in its process group go with it, like hooks or a
            // runc init which did not set up the container yet, which would
            // be left running otherwise. A container init which started a
            // session of its own syncs with its runtime over a socket, and
            // fails once that closes. The runtime itself gets killed as well,
            // it may have moved to another group.
            if let Err(e) = child.kill_group() {
                debug!("Unable to kill process group of runtime: {e}");
            }
            if let Err(e) = child.start_kill() {
                debug!("Unable to kill runtime: {e}");
            }
            // Its container process gets reparented to this server only once
            // it is gone, the cleanup relies on that. A runtime stuck in the
            // kernel is not waited for forever.
            let reaped = child.reap();
            let (grace, stopped) = (Self::RUNTIME_GRACE, Self::STOPPED_EXIT_WAIT);
            match wait_unless_stopped(reaped, grace, stop, stopped).await {
                Some(Ok(_)) => {}
                Some(Err(e)) => debug!("Unable to wait for runtime: {e}"),
                None => warn!("Killed runtime did not exit, cleaning up anyway"),
            }
        }
        // Dropped before its mark: the drop kills the group of a runtime
        // which did not exit, while the mark still keeps the orphan sweep
        // from reaping it and its group number from getting reused. Neither
        // is held while waiting for the output.
        let pending = child.pending.take();
        drop(child);
        drop(pending);

        // Its stderr usually holds the reason.
        RuntimeOutput::finish_unless_stopped(output, stop).await
    }

    /// Clean up after a failed runtime call and build its error, with the
    /// stderr of the runtime appended.
    fn fail_create(&self, pidfile: &Path, error: anyhow::Error, stderr: &[u8]) -> anyhow::Error {
        // Do not leave a container process behind which the runtime created
        // before it failed, and reap what else it left.
        self.cleanup_failed_grandchild(pidfile);
        self.sweep.notify_one();

        let error = with_stderr(error, stderr);
        // The caller logs the error it returns.
        debug!("Failed: {error:#}");
        error
    }

    /// Kill and reap the container process of a failed runtime call, if the
    /// runtime got as far as writing its PID. It got reparented to this
    /// subreaper and nobody else waits for it.
    ///
    /// Only this PID is waited for. Reaping any exited child instead would take
    /// the exit status of runtimes of concurrent calls, or of grandchildren
    /// which are not registered yet.
    pub(super) fn cleanup_failed_grandchild(&self, pidfile: &Path) {
        let Ok(pid) = read_pid_file(pidfile) else {
            return;
        };
        // A stale PID file may name a process which got reused by a container
        // or exec of this server, which must not be killed.
        if self.is_claimed(pid) {
            debug!("Not cleaning up PID {pid} of failed runtime call, it is in use");
            return;
        }

        // Only a child, the PID may have been reused otherwise. With its
        // group, like on a timeout, so nothing it started survives.
        if let Err(e) = Process::child(pid).signal_group(Signal::SIGKILL) {
            debug!("Not cleaning up PID {pid} of failed runtime call: {e}");
            return;
        }
        debug!("Cleaning up PID {pid} of failed runtime call");
        task::spawn(wait_for_exit_code(pid));
    }
}

/// Wait for `future` for at most `limit`, and only for `stopped` more once
/// the shutdown stopped the call through `stop`, see
/// [`ChildReaper::STOP_BUDGET`]. None if it did not finish by then.
async fn wait_unless_stopped<T>(
    future: impl Future<Output = T>,
    limit: Duration,
    stop: &CancellationToken,
    stopped: Duration,
) -> Option<T> {
    let deadline = Instant::now() + limit;
    let mut future = pin!(future);
    select! {
        biased;
        res = &mut future => return Some(res),
        () = time::sleep_until(deadline) => return None,
        () = stop.cancelled() => {}
    }
    time::timeout_at(deadline.min(Instant::now() + stopped), future)
        .await
        .ok()
}

/// A runtime call which failed before its runtime exited, see
/// [`ChildReaper::stop_runtime`].
struct RuntimeFailure {
    error: anyhow::Error,

    /// How long the runtime may take to exit on its own, none once its
    /// deadline passed.
    grace: Duration,
}

impl RuntimeFailure {
    fn new(error: anyhow::Error) -> Self {
        Self {
            error,
            grace: ChildReaper::RUNTIME_GRACE,
        }
    }

    fn timed_out() -> Self {
        Self {
            error: format_err!("runtime did not finish in time"),
            grace: Duration::ZERO,
        }
    }

    /// The shutdown stopped the call, see [`ChildReaper::stop_calls`].
    fn stopped() -> Self {
        Self {
            error: format_err!("runtime got stopped by the shutdown"),
            grace: Duration::ZERO,
        }
    }
}

/// A runtime whose process group gets killed if it is dropped before it got
/// reaped, like a call in progress which the shutdown drops once it stops
/// waiting for it. `kill_on_drop` only kills the runtime, its helpers in its
/// group, like hooks or a runc init which did not set up the container yet,
/// would be left running, see [`ChildReaper::stop_runtime`]. Until it got
/// reaped, the ID of the group is still the one of the runtime.
#[derive(Debug)]
struct Runtime {
    child: RuntimeChild,

    /// Keeps the runtime pending until it got reaped, so that the orphan
    /// sweep leaves its exit status to tokio. Its PID may belong to another
    /// process after that, which another mark may keep pending.
    pending: Option<PendingPid>,
}

impl Deref for Runtime {
    type Target = RuntimeChild;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl DerefMut for Runtime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Runtime {
    /// Wait for the exit of the runtime, which reaps it, and end its pending
    /// mark right away. Returns the status again once it got reaped.
    async fn reap(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            self.pending = None;
        }
        status
    }

    /// Kill the process group of the runtime, which it leads, see
    /// `spawn_runtime`. Nothing once it got reaped, the ID of the group may
    /// belong to another one then, the runtime has no ID anymore.
    fn kill_group(&self) -> nix::Result<()> {
        let Ok(pid) = child_pid(self.child.id()) else {
            return Ok(());
        };
        killpg(pid, Signal::SIGKILL)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Err(e) = self.kill_group() {
            debug!("Unable to kill process group of dropped runtime: {e}");
        }
    }
}
