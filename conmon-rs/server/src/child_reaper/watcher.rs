//! Watching a registered child until it exited, and what follows.

use super::{Child, ChildKind, EXIT_CODE_TIMED_OUT, ExitChannelData, ReapableChild};
use crate::{
    attach::SharedContainerAttach,
    container_io::{DRAIN_MAX_LOG_WAIT, Drain, Message},
    container_log::{ContainerLog, SharedContainerLog},
    fs_util,
    oom_watcher::{OOMEvent, OOMWatcher},
    process::{self, Process},
};
use anyhow::{Context, Result};
use nix::sys::signal::Signal;
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::broadcast::{self, Receiver},
    task,
    time::{self, Instant},
};
use tracing::{Instrument, debug, debug_span, error, info, trace, warn};

/// Runs a closure once dropped, at the end of a task however it ends.
struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// What only the watcher of a child needs, which moves into it, see
/// [`ReapableChild::watch`].
#[derive(Debug)]
pub(crate) struct ExitHandling {
    exit_paths: Vec<PathBuf>,
    oom_exit_paths: Vec<PathBuf>,

    /// When the process gets killed, only an exec_sync has one.
    timeout: Option<Instant>,
}

impl ExitHandling {
    /// Write neither exit nor OOM exit files, for a child whose call fails.
    pub(super) fn drop_exit_paths(&mut self) {
        self.exit_paths.clear();
        self.oom_exit_paths.clear();
    }
}

impl ReapableChild {
    /// Whether the process which exited with `exit_code` got OOM killed. The
    /// watcher writes the files for the OOMs it could tell on its own, this
    /// the ones which only the exit status tells.
    pub(super) async fn handle_oom(
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
    /// False if the deadline passed, which got logged here.
    pub(super) async fn wait_for_output(attach: SharedContainerAttach, deadline: Instant) -> bool {
        let (mut rx, ended) = attach.subscribe();
        if ended {
            return true;
        }
        let mut drain = Drain::default();
        loop {
            if time::timeout_at(deadline, attach.wait_logged())
                .await
                .is_err()
            {
                warn!("Log drivers did not take the output in time, not waiting for them");
                return false;
            }
            // A read loop which got output while the drain waited gives it
            // another chance.
            let Ok(output) = time::timeout_at(deadline, drain.recv_output(&mut rx, None)).await
            else {
                warn!("Output did not end in time, not waiting for it");
                return false;
            };
            if output.is_none() && (drain.exhausted() || !attach.is_logging()) {
                return true;
            }
        }
    }

    /// Wait for the output like [`ReapableChild::wait_for_output`], then
    /// stop the read loops and wait for them to end, which writes the lines
    /// still waiting for their ending into the log, and sync the log to
    /// disk. Nothing reaches the log after the exit file this way. A log
    /// write or sync which hangs must not hold the exit file back, all of it
    /// takes [`DRAIN_MAX_LOG_WAIT`] at most.
    pub(super) async fn finish_output(attach: SharedContainerAttach, logger: &SharedContainerLog) {
        let deadline = Instant::now() + DRAIN_MAX_LOG_WAIT;
        let logged = Self::wait_for_output(attach.clone(), deadline).await;
        attach.stop_output();
        let (mut rx, ended) = attach.subscribe();
        if !ended {
            // Without read loops, the output never ends and nothing got
            // logged.
            if !attach.has_output() {
                return;
            }
            let end = async {
                while !matches!(
                    rx.recv().await,
                    Ok(Message::Done) | Err(broadcast::error::RecvError::Closed)
                ) {}
            };
            // The read loops get until the deadline to end, the sync would
            // get no time after it. A wait for the output which ran into the
            // deadline, because the output did not end or the log drivers
            // hang, logged that already, the read loops hardly end then.
            if time::timeout_at(deadline, end).await.is_err() {
                if logged {
                    warn!("Read loops did not end in time, not waiting for them");
                }
                return;
            }
        }
        match time::timeout_at(deadline, ContainerLog::sync(logger)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("Unable to sync the log: {e:#}"),
            Err(_) => warn!("Log did not sync in time, not waiting for it"),
        }
    }

    /// Split the child into the registered part and what only its watcher
    /// needs.
    pub(crate) fn from_child(child: Child) -> (Box<str>, Self, ExitHandling) {
        static NEXT_SEQ: AtomicU64 = AtomicU64::new(0);
        let Child {
            id,
            kind,
            pid,
            exit_paths,
            oom_exit_paths,
            timeout,
            io,
            token,
            process,
        } = child;
        (
            id,
            Self {
                seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
                pid,
                // Not reaped before its watcher runs, the PID is its own.
                process: process.unwrap_or_else(|| Arc::new(Process::open(pid))),
                reaped: Arc::default(),
                io: (kind == ChildKind::Container).then_some(io),
                token,
            },
            ExitHandling {
                exit_paths,
                oom_exit_paths,
                timeout,
            },
        )
    }

    /// Wait for the exit of the process with `wait`, or kill it at the
    /// `timeout`, and return its exit code and whether it timed out. The
    /// wait stays pending on the timeout, the watcher reaps the process with
    /// it once the exit went out, see [`ReapableChild::reap_killed`].
    async fn wait_or_kill(
        process: &Process,
        mut wait: Pin<&mut impl Future<Output = i32>>,
        timeout: Option<Instant>,
    ) -> (i32, bool) {
        let Some(timeout) = timeout else {
            return (wait.await, false);
        };
        if let Ok(code) = time::timeout_at(timeout, wait.as_mut()).await {
            return (code, false);
        }
        match process.signal_group(Signal::SIGKILL) {
            Ok(()) => info!(
                pid = %process.pid(),
                "Killed the process, it did not exit before its timeout"
            ),
            Err(e) => debug!("Not killing timed out process, it is gone: {e}"),
        }
        (EXIT_CODE_TIMED_OUT, true)
    }

    /// Reap the process which got killed at its timeout through the `wait`
    /// which timed out, once the SIGKILL took effect. A second wait would
    /// register the pidfd again, or race this one, whose blocking wait still
    /// runs without a pidfd, and could take the exit of another child which
    /// reused the PID. A process stuck in the kernel, like on a hung network
    /// mount, may take long, it keeps only a pidfd busy meanwhile, not a
    /// thread. The process stays registered until then, so that the orphan
    /// sweep leaves it to this wait and the shutdown waits for it and
    /// reports it if it does not exit in time.
    async fn reap_killed(wait: Pin<&mut impl Future<Output = i32>>) {
        let code = wait.await;
        debug!(code, "Timed out process got reaped");
    }

    /// Watch the process and return the receiver for its exit, which exists
    /// before the task starts, a receiver subscribed after the exit would
    /// miss it. `forget` runs once the exit got sent and the process got
    /// reaped, which comes later for one killed at its timeout, or the task
    /// ended otherwise, like with a panic. A task which never ran, which
    /// tokio drops right in `spawn` while the runtime shuts down, does not
    /// run it, so the caller may hold a lock which `forget` takes.
    pub(super) fn watch(
        &self,
        exit: ExitHandling,
        forget: impl FnOnce() + Send + 'static,
    ) -> Receiver<ExitChannelData> {
        let ExitHandling {
            exit_paths,
            oom_exit_paths,
            timeout,
        } = exit;
        let pid = self.pid();
        let process = self.process.clone();
        let reaped = self.reaped.clone();
        // Only one exit code will be written.
        let (exit_tx, exit_rx) = broadcast::channel(1);
        let token = self.token.clone();
        // None for an exec, see `io`.
        let io = self.io.clone();

        task::spawn(
            async move {
                // Only created on the first poll, see above.
                let _forget = OnDrop(Some(forget));
                // Nobody learns about an OOM without the files, like an exec.
                let oom = async {
                    if oom_exit_paths.is_empty() {
                        return None;
                    }
                    OOMWatcher::watch(&token, pid, &oom_exit_paths)
                        .instrument(debug_span!("cgroup_handling", %pid))
                        .await
                };
                let mut reap =
                    pin!(process::wait_for_exit(&process).instrument(debug_span!("wait_for_exit")));
                let wait = async {
                    let (exit_code, timed_out) =
                        Self::wait_or_kill(&process, reap.as_mut(), timeout).await;
                    // One killed at its timeout gets reaped at the end of
                    // the watcher, which forgets it right after.
                    reaped.store(!timed_out, Ordering::Release);
                    // Tells the users of the token that the process is gone,
                    // which ends the OOM watcher.
                    token.cancel();
                    (exit_code, timed_out)
                };
                let ((exit_code, timed_out), event) = tokio::join!(wait, oom);
                let oomed = Self::handle_oom(event, exit_code, &oom_exit_paths).await;

                // The exit file tells that the log is complete, so let the read
                // loops write what the process left in the pipes, and stop
                // them, see `stop_output`. The output of an exec gets stopped
                // by its consumer, see `StopOutput`.
                if let Some(io) = &io {
                    Self::finish_output(io.attach().clone(), io.logger()).await;
                }

                let exit_channel_data = ExitChannelData {
                    exit_code,
                    timed_out,
                };
                // Only of a container: a node runs thousands of execs, like
                // the ones of probes. With the PID, which the lines of the
                // shutdown name for the processes which did not exit.
                if io.is_some() {
                    info!(%pid, exit_code, oomed, "Container exited");
                } else {
                    debug!(exit_code, timed_out, "Exec process exited");
                }
                Self::write_to_exit_paths(exit_code, &exit_paths).await;

                // Only the consumer of an exec receives it, nobody of a
                // container.
                if exit_tx.send(exit_channel_data).is_err() {
                    trace!("Nobody receives the exit status");
                }
                // Only after the exit went out, so that the client gets its
                // timeout right away.
                if timed_out {
                    Self::reap_killed(reap).await;
                }
            }
            .instrument(debug_span!("watch", %pid))
            // The span of the request or the session which started the
            // process names its container, see `enter_request` of the RPC
            // interface. Below the debug level, the span above is disabled
            // and would not hand it on.
            .in_current_span(),
        );
        exit_rx
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
                    error!("Unable to write exit file: {e:#}");
                }
            }
            .instrument(span)
            .await;
        }
    }

    /// Write the exit file at once, like conmon does. CRI-O reads it as soon
    /// as it gets created, which must not find it empty. CRI-O ignores the
    /// temporary file, which is not named like a container. A symlink in its
    /// place does not get followed, like for the log files, the file gets
    /// created new.
    pub(super) async fn write_exit_file(path: &Path, contents: &str) -> Result<()> {
        let name = path.file_name().context("exit path has no file name")?;
        let mut tmp_name = OsString::from(".");
        tmp_name.push(name);
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        let result = match Self::write_new_file(&tmp, contents).await {
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

    /// Create the file new with the contents. One left behind at the path,
    /// like by a crash, gets removed first, a symlink is not followed.
    async fn write_new_file(path: &Path, contents: &str) -> io::Result<()> {
        fs_util::remove_file(path)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .await?;
        file.write_all(contents.as_bytes()).await?;
        // The write runs in the background until flushed, the rename must
        // not come first.
        file.flush().await
    }
}
