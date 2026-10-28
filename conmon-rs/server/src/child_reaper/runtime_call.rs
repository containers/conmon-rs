//! Running a runtime for a call: its output, its process group and the
//! cleanup after a failure.

use super::{ChildReaper, InFlight, PendingPid};
use crate::{
    child::sleep_until,
    container_io::{ContainerIO, ContainerIOType},
    fd_mapping::{CommandFdExt, FdMapping},
    init,
    mutex::lock,
    process::{Process, child_pid, read_pid_file, wait_for_exit_code},
    tail::Tail,
};
use anyhow::{Context, Result, format_err};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::{
    io,
    ops::{Deref, DerefMut},
    os::fd::{OwnedFd, RawFd},
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
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
    pub(super) const CONNECT_AFTER_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

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

        // Only a child, the PID may have been reused otherwise.
        if let Err(e) = Process::child(pid).signal(Signal::SIGKILL) {
            debug!("Not cleaning up PID {pid} of failed runtime call: {e}");
            return;
        }
        debug!("Cleaning up PID {pid} of failed runtime call");
        task::spawn(wait_for_exit_code(CancellationToken::new(), pid));
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
