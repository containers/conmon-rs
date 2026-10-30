//! Exec sessions, which start a process in the container for their client.

use super::{
    StreamingServer,
    session::{ClientRx, InputEvent, ResizeRx, SessionInput},
};
use crate::conmon_capnp::conmon::CgroupManager;
use crate::{
    attach::{SharedContainerAttach, StopOutput},
    child_reaper::{ChildReaper, ExitChannelData, RuntimeCall, WatchedExec},
    config::Config,
    container_io::{ContainerIO, Drain, EarlyStdio, Message as IOMessage, SharedContainerIO},
    container_log::ContainerLog,
    process::Process,
    runtime_args::GenerateRuntimeArgs,
    server::Server,
    streaming_server::remotecommand::{
        MESSAGE_TOO_LARGE, Operation, Output, STDERR_BYTE, STDOUT_BYTE, Status,
    },
    terminal::{Terminal, TerminalResizer},
};
use anyhow::{Context, Result, bail};
use async_channel::Receiver as IOReceiver;
use bytes::Bytes;
use nix::sys::signal::Signal;
use std::{
    collections::VecDeque,
    future::{self, Future},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::broadcast::{self, error::RecvError},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// The largest frame of the output collected while the runtime of an exec
/// ran, like the largest reads of the pipes.
const EARLY_FRAME_SIZE: usize = 32 * 1024;

/// Required exec session data. Not `Debug`, the command may carry secrets.
pub struct ExecSession {
    pub child_reaper: Arc<ChildReaper>,

    /// Whether the process gets a terminal. Its console socket gets created
    /// only once the client came.
    pub tty: bool,
    pub server_config: Arc<Config>,
    pub cgroup_manager: CgroupManager,
    pub container_id: Box<str>,
    pub command: Vec<String>,
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub idle_timeout: Option<Duration>,
}

impl StreamingServer {
    /// Run the runtime of an exec session and register the process it
    /// started. A terminal gets the sizes of the client from `resize`
    /// meanwhile.
    pub(super) async fn start_exec(session: ExecSession, resize: ResizeRx) -> Result<StartedExec> {
        let mut container_io =
            ContainerIO::new(session.tty, ContainerLog::new()).context("create container IO")?;
        let pidfile = ContainerIO::temp_file(
            Some(session.server_config.runtime_dir()),
            "exec_streaming",
            ".pid",
        );

        let args = GenerateRuntimeArgs {
            config: &session.server_config,
            id: &session.container_id,
            container_io: &container_io,
            pidfile: pidfile.path(),
            cgroup_manager: session.cgroup_manager,
        };
        let args = args
            .exec_args(&session.command)
            .context("generate exec args")?;

        // Read while the runtime runs, see `ContainerIO::collect_stdio`.
        container_io
            .collect_stdio(session.stdout, session.stderr)
            .context("collect exec output")?;
        let stdio = container_io
            .stdio()
            .context("retrieve stdout and stderr channels")?;

        // Clients send their size right away. The runtime connects the
        // terminal before it starts the command, so applying the size while
        // it runs gives the command its size from the start, usually, like
        // with CRI-O, not only once the loop of the session runs.
        let resizer = container_io.terminal().map(Terminal::resizer);
        let create = session.child_reaper.create_child(
            RuntimeCall {
                command_start: Some(args.command_start),
                stdin: session.stdin,
                ..RuntimeCall::new(
                    session.server_config.runtime(),
                    &args.args,
                    pidfile.path(),
                    // The process has no timeout, a runtime which hangs
                    // still gets killed.
                    Server::exec_runtime_deadline(None),
                )
            },
            &mut container_io,
        );
        let res = Self::resizing(create, resize, resizer).await;
        let started = match res {
            Ok(started) => started,
            Err(e) => {
                let e = container_io.runtime_error(e).await;
                return Err(e.context("create new child process"));
            }
        };
        // The runtime wrote the PID, the file served its purpose.
        drop(pidfile);

        let WatchedExec {
            io,
            token,
            process,
            exit_rx,
            stop_output,
        } = session
            .child_reaper
            .watch_exec(session.container_id, started, None, container_io)
            .context("watch grandchild for pid")?;
        let teardown = ExecTeardown {
            stdio,
            _stop_output: stop_output,
            attach: io.attach().clone(),
            token,
            process,
            kill: Arc::default(),
        };
        let early_stdio = io.take_early_stdio().await;
        Ok(StartedExec {
            io,
            early_stdio,
            exit_rx,
            stdin_enabled: session.stdin,
            stdout_enabled: session.stdout,
            stderr_enabled: session.stderr,
            teardown,
        })
    }

    /// The exec specific read/write loop. `status` gets the exit status once
    /// the process exited, which goes out after the loop.
    pub(super) async fn exec_loop(
        started: StartedExec,
        sender: &mut Output,
        client: ClientRx,
        status: &mut Option<Status>,
    ) -> Result<()> {
        let StartedExec {
            io,
            early_stdio,
            mut exit_rx,
            stdin_enabled,
            stdout_enabled,
            stderr_enabled,
            teardown,
        } = started;
        let (stdout_rx, stderr_rx) = teardown.stdio.clone();
        let mut input = SessionInput::new(io, teardown.attach.clone(), client, stdin_enabled);

        // Streams which are disabled or whose read loop signalled the end are
        // not polled anymore. A disabled one is not read at all, so let its
        // read loop discard the output instead of waiting.
        let mut stdout_done = !stdout_enabled;
        let mut stderr_done = !stderr_enabled;
        if stdout_done {
            stdout_rx.close();
        }
        if stderr_done {
            stderr_rx.close();
        }

        // What the process wrote while the runtime ran comes first, in the
        // loop, so that a slow client does not hold up the stdin, resizing
        // and the exit meanwhile. Output which got dropped for its size would
        // leave a gap, so the stream ends with it, and the session fails.
        let truncated = early_stdio.stdout.truncated || early_stdio.stderr.truncated;
        let (mut early_stdout, mut early_stderr) = (VecDeque::new(), VecDeque::new());
        for (early, rx, frames, done) in [
            (
                early_stdio.stdout,
                &stdout_rx,
                &mut early_stdout,
                &mut stdout_done,
            ),
            (
                early_stdio.stderr,
                &stderr_rx,
                &mut early_stderr,
                &mut stderr_done,
            ),
        ] {
            if *done {
                continue;
            }
            *frames = Self::early_frames(early.data);
            if early.done || early.truncated {
                *done = true;
                rx.close();
            }
        }

        loop {
            tokio::select! {
                () = future::ready(()), if !early_stdout.is_empty() || !early_stderr.is_empty() => {
                    for (frames, stream_byte) in [(&mut early_stdout, STDOUT_BYTE), (&mut early_stderr, STDERR_BYTE)] {
                        if let Some(frame) = frames.pop_front() {
                            sender.data(stream_byte, &frame).await?;
                        }
                    }
                },

                res = stdout_rx.recv(), if !stdout_done && early_stdout.is_empty() => match res {
                    Ok(IOMessage::Data(data, _)) => {
                        sender.data(STDOUT_BYTE, &data).await?;
                    }
                    Ok(IOMessage::Done) | Err(_) => stdout_done = true,
                },

                res = stderr_rx.recv(), if !stderr_done && early_stderr.is_empty() => match res {
                    Ok(IOMessage::Data(data, _)) => {
                        sender.data(STDERR_BYTE, &data).await?;
                    }
                    Ok(IOMessage::Done) | Err(_) => stderr_done = true,
                },

                event = input.next() => match event {
                    InputEvent::Resize(data) => input.resize(&data),
                    InputEvent::Ping => sender.ping().await?,
                    // Close the process stdin as well and keep forwarding the
                    // output until it exits.
                    InputEvent::StdinClosed => input.close_container_stdin(),
                    // The client closed the connection, everything it sent
                    // reached the process.
                    InputEvent::Ended => break,
                    InputEvent::TooLarge => bail!(MESSAGE_TOO_LARGE),
                },

                res = exit_rx.recv() => {
                    // Nothing ends the process otherwise, kill it like after
                    // the idle timeout.
                    if res.is_err() {
                        teardown.kill.store(true, Ordering::Release);
                    }
                    *status = Some(Self::exit_status(res, truncated));
                    // Forward what the read loops still deliver before closing.
                    for (frames, stream_byte) in [(&mut early_stdout, STDOUT_BYTE), (&mut early_stderr, STDERR_BYTE)] {
                        for frame in frames.drain(..) {
                            sender.data(stream_byte, &frame).await?;
                        }
                    }
                    if !stdout_done {
                        Self::drain_stdio(&stdout_rx, STDOUT_BYTE, sender).await?;
                    }
                    if !stderr_done {
                        Self::drain_stdio(&stderr_rx, STDERR_BYTE, sender).await?;
                    }
                    break
                },
            }
        }

        Ok(())
    }

    /// The status of the exit the watcher of the process sent. It sends it
    /// once, and only goes away without it if its task got aborted, the
    /// session must not wait for the exit forever then. A panic aborts the
    /// whole server in release builds. A session whose output got
    /// `truncated` while the runtime ran fails, the client must not take
    /// output with a gap for the complete one, see
    /// [`ContainerIO::collect_stdio`].
    fn exit_status(res: Result<ExitChannelData, RecvError>, truncated: bool) -> Status {
        match res {
            Ok(exit_data) if truncated => Status::failed(
                Operation::Exec,
                format_args!(
                    "the command wrote more than {} MiB while the runtime started it, \
                     the rest of it got dropped (exit code {})",
                    ContainerIO::MAX_STDIO_STREAM_SIZE / (1024 * 1024),
                    exit_data.exit_code,
                ),
            ),
            Ok(exit_data) => Status::exited(exit_data.exit_code),
            Err(e) => Status::failed(
                Operation::Exec,
                format_args!("unable to get the exit of the command: {e}"),
            ),
        }
    }

    /// Run the runtime and give the terminal the sizes of the client
    /// meanwhile, which it keeps until the runtime connected it, see
    /// [`TerminalResizer::resize`]. The loop of the session takes over the
    /// resizing once the runtime ran.
    async fn resizing<F: Future>(
        run: F,
        mut resize: ResizeRx,
        resizer: Option<TerminalResizer>,
    ) -> F::Output {
        let Some(resizer) = resizer else {
            return run.await;
        };
        tokio::pin!(run);
        loop {
            tokio::select! {
                output = &mut run => return output,
                // A client which went away sends nothing anymore.
                Ok(()) = resize.changed() => {
                    if let Some(data) = resize.borrow_and_update().clone() {
                        SessionInput::resize_with(&data, |width, height| {
                            resizer.resize(width, height).map(|()| true)
                        });
                    }
                }
            }
        }
    }

    /// Split the output collected while the runtime ran into frames, without
    /// copying it.
    fn early_frames(data: Vec<u8>) -> VecDeque<Bytes> {
        let mut data = Bytes::from(data);
        let mut frames = VecDeque::with_capacity(data.len().div_ceil(EARLY_FRAME_SIZE));
        while !data.is_empty() {
            frames.push_back(data.split_to(data.len().min(EARLY_FRAME_SIZE)));
        }
        frames
    }

    /// Forward the remaining output of an exec stdio channel after the process
    /// exited, until the read loop signals that it is done or the [`Drain`]
    /// gives up.
    async fn drain_stdio(
        rx: &IOReceiver<IOMessage>,
        stream_byte: u8,
        sender: &mut Output,
    ) -> Result<()> {
        let mut drain = Drain::default();
        loop {
            match drain.recv(rx.recv()).await {
                Some(Ok(IOMessage::Data(data, _))) => {
                    sender.data(stream_byte, &data).await?;
                }
                Some(Ok(IOMessage::Done)) | Some(Err(_)) | None => return Ok(()),
            }
        }
    }
}

/// An exec process which got started and registered for a session.
pub(super) struct StartedExec {
    io: SharedContainerIO,

    /// The output written while the runtime ran.
    early_stdio: EarlyStdio,
    exit_rx: broadcast::Receiver<ExitChannelData>,
    stdin_enabled: bool,
    stdout_enabled: bool,
    stderr_enabled: bool,
    pub(super) teardown: ExecTeardown,
}

/// Ends an exec session however it ends, also if the client went away and
/// the loop got aborted, or before the process even got started. The loop
/// ends before the exit of the process if the connection is gone, if the
/// client sent a message which is too large, or if the session hit the idle
/// timeout or lost the exit. Only the last two kill the process, see
/// [`ExecTeardown::kill`], the others close its stdin or hang up its
/// terminal. All but the hang-up stop the output, which nothing takes
/// anymore.
pub(super) struct ExecTeardown {
    /// The stdout and stderr channels of the process.
    stdio: (IOReceiver<IOMessage>, IOReceiver<IOMessage>),
    attach: SharedContainerAttach,

    /// Cancelled once the process exited.
    token: CancellationToken,

    /// The process, which its watcher may reap before the token got
    /// cancelled, so it does not get signalled by its PID.
    process: Process,

    /// Set once the session ended with the idle timeout or lost the exit,
    /// which kills the process, like the reaper kills one after its timeout.
    /// Without a session the process would run on with nothing waiting for
    /// it. Otherwise it gets the end of its stdin and its output pipes
    /// closed, or the hang-up of its terminal, like a process started by the
    /// client itself once the client went away.
    pub(super) kill: Arc<AtomicBool>,

    /// Stops the read loops once the session took the output, after the
    /// exit, dropped after the rest.
    _stop_output: StopOutput,
}

impl Drop for ExecTeardown {
    fn drop(&mut self) {
        // Nothing forwards the output anymore, the read loops discard it
        // instead of waiting for this session.
        let (stdout_rx, stderr_rx) = &self.stdio;
        stdout_rx.close();
        stderr_rx.close();

        if self.token.is_cancelled() {
            return;
        }
        if self.kill.load(Ordering::Acquire) {
            let pid = self.process.pid();
            debug!("Killing exec process {pid}, its session ended");
            if let Err(e) = self.process.signal_group(Signal::SIGKILL) {
                debug!("Not killing exec process {pid}, it is gone: {e}");
            }
            // A lost exit never cancels the token, which `StopOutput` waits
            // for.
            self.attach.stop_output();
        } else if self.attach.close_container_stdin() {
            // Without a client the process would wait for stdin forever.
            // Close it, like the end of the session would for a process
            // started by the client itself. Nothing takes the output
            // anymore either, the read loops stop and close the pipes, so
            // that the writes of a process like `tail -f`, which never reads
            // stdin, fail.
            self.attach.stop_output();
        } else {
            // A terminal cannot be closed without ending its output, hang it
            // up instead, like a closed connection does. The stdin loop
            // keeps its master open, so its output keeps getting read until
            // the exit: a process which ignores the hang-up would block on
            // a full terminal otherwise.
            self.hang_up();
        }
    }
}

impl ExecTeardown {
    /// Hang up the terminal like a closed connection does: signal the process
    /// group and the process. Only while the process was not reaped yet, its
    /// group may be gone and taken by another one otherwise, see
    /// [`Process::signal_group`].
    fn hang_up(&self) {
        let pid = self.process.pid();
        debug!("Hanging up the terminal of exec process {pid}");
        if let Err(e) = self.process.signal_group(Signal::SIGHUP) {
            debug!("Not hanging up exec process {pid}, it is gone: {e}");
        }
    }
}

#[cfg(test)]
mod tests;
