//! Watching a registered child until it exited, and what follows.

use super::{EXIT_CODE_TIMED_OUT, ExitChannelData, ReapableChild, kill_grandchild};
use crate::{
    attach::SharedContainerAttach,
    child::{Child, ChildKind},
    container_io::{DRAIN_MAX_LOG_WAIT, Drain, Message},
    init,
    oom_watcher::{OOMEvent, OOMWatcher},
    process,
};
use anyhow::{Context, Result};
use nix::{sys::signal::Signal, unistd::Pid};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::{
    fs,
    process::Command,
    sync::broadcast::{self, Receiver},
    task,
    time::{self, Instant},
};
use tracing::{Instrument, debug, debug_span, error, warn};

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
    cleanup_cmd: Vec<String>,
}

impl ReapableChild {
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
    pub(super) async fn wait_for_output(attach: SharedContainerAttach, deadline: Instant) {
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
            let Ok(output) = time::timeout_at(deadline, drain.recv_output(&mut rx, None)).await
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
    pub(super) async fn finish_output(attach: SharedContainerAttach) {
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

    /// Split the child into the registered part and what only its watcher
    /// needs.
    pub fn from_child(child: Child) -> (Box<str>, Self, ExitHandling) {
        static NEXT_SEQ: AtomicU64 = AtomicU64::new(0);
        let Child {
            id,
            kind,
            pid,
            exit_paths,
            oom_exit_paths,
            timeout,
            io,
            cleanup_cmd,
            token,
        } = child;
        (
            id,
            Self {
                seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
                kind,
                pid,
                io,
                token,
            },
            ExitHandling {
                exit_paths,
                oom_exit_paths,
                timeout,
                cleanup_cmd,
            },
        )
    }

    /// Wait for the exit of the process, or kill it at the `timeout`, and
    /// return its exit code and whether it timed out. The wait gets dropped
    /// on the timeout, the process gets reaped in the background once the
    /// SIGKILL took effect. A process stuck in the kernel keeps only a pidfd
    /// busy then, not a thread.
    async fn wait_or_kill(pid: Pid, timeout: Option<Instant>) -> (i32, bool) {
        let wait = process::wait_for_exit_code(pid).instrument(debug_span!("wait_for_exit_code"));
        let Some(timeout) = timeout else {
            return (wait.await, false);
        };
        if let Ok(code) = time::timeout_at(timeout, wait).await {
            return (code, false);
        }
        kill_grandchild(pid, Signal::SIGKILL);
        task::spawn(process::wait_for_exit_code(pid));
        (EXIT_CODE_TIMED_OUT, true)
    }

    /// Watch the process and return the receiver for its exit, which exists
    /// before the task starts, a receiver subscribed after the exit would
    /// miss it. `forget` runs once the exit got sent, or the task ended
    /// otherwise, like with a panic. A task which never ran, which tokio
    /// drops right in `spawn` while the runtime shuts down, does not run it,
    /// so the caller may hold a lock which `forget` takes.
    pub(super) fn watch(
        &self,
        exit: ExitHandling,
        forget: impl FnOnce() + Send + 'static,
    ) -> Receiver<ExitChannelData> {
        let ExitHandling {
            exit_paths,
            oom_exit_paths,
            timeout,
            cleanup_cmd,
        } = exit;
        let pid = self.pid();
        // Only one exit code will be written.
        let (exit_tx, exit_rx) = broadcast::channel(1);
        let token = self.token().clone();
        let io = self.io.clone();
        let kind = self.kind;

        task::spawn(
            async move {
                // Only created on the first poll, see above.
                let _forget = OnDrop(Some(forget));
                debug!("Running task");
                // Nobody learns about an OOM without the files, like an exec.
                let oom = async {
                    if oom_exit_paths.is_empty() {
                        return None;
                    }
                    OOMWatcher::watch(&token, pid, &oom_exit_paths)
                        .instrument(debug_span!("cgroup_handling", %pid))
                        .await
                };
                let wait = async {
                    let exit = Self::wait_or_kill(pid, timeout).await;
                    // Tells the users of the token that the process is gone,
                    // which ends the OOM watcher.
                    token.cancel();
                    exit
                };
                let ((exit_code, timed_out), event) = tokio::join!(wait, oom);
                let oomed = Self::handle_oom(event, exit_code, &oom_exit_paths).await;

                // The exit file tells that the log is complete, so let the read
                // loops write what the process left in the pipes, and stop
                // them, see `stop_output`. The output of an exec gets stopped
                // by its consumer, see `StopOutput`.
                if kind == ChildKind::Container {
                    Self::finish_output(io.attach().clone()).await;
                }

                let exit_channel_data = ExitChannelData {
                    exit_code,
                    timed_out,
                };
                debug!(?exit_paths, oomed, "Write to exit paths");
                Self::write_to_exit_paths(exit_code, &exit_paths).await;

                if !cleanup_cmd.is_empty() {
                    Self::spawn_cleanup_process(&cleanup_cmd);
                }

                debug!("Sending exit struct to channel: {:?}", exit_channel_data);
                if exit_tx.send(exit_channel_data).is_err() {
                    debug!("Unable to send exit status");
                }
                debug!("Task done");
            }
            .instrument(debug_span!("watch", %pid)),
        );
        exit_rx
    }

    fn spawn_cleanup_process(raw_cmd: &[String]) {
        let mut cleanup_cmd = Command::new(&raw_cmd[0]);
        init::reset_inherited(&mut cleanup_cmd);

        cleanup_cmd.args(&raw_cmd[1..]).stdin(Stdio::null());

        tokio::spawn(async move {
            match cleanup_cmd.status().await {
                Ok(status) if !status.success() => {
                    error!("Unable to run cleanup command: {status}");
                }
                Ok(_) => {}
                Err(e) => error!("Unable to run cleanup command: {e:#}"),
            }
        });
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
    pub(super) async fn write_exit_file(path: &Path, contents: &str) -> Result<()> {
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
