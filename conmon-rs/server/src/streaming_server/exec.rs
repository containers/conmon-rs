//! Exec sessions, which start a process in the container for their client.

use super::{
    StreamingServer,
    session::{ClientRx, InputEvent, SessionInput},
};
use crate::conmon_capnp::conmon::CgroupManager;
use crate::{
    attach::{SharedContainerAttach, StopOutput},
    child::{Child, ChildKind},
    child_reaper::{ChildReaper, ExitChannelData, RuntimeCall},
    config::Config,
    container_io::{ContainerIO, Drain, EarlyStdio, Message as IOMessage, SharedContainerIO},
    container_log::ContainerLog,
    process::Process,
    remotecommand::{MESSAGE_TOO_LARGE, Output, STDERR_BYTE, STDOUT_BYTE, Status},
    server::{GenerateRuntimeArgs, Server},
};
use anyhow::{Context, Result, bail};
use async_channel::Receiver as IOReceiver;
use bytes::Bytes;
use nix::sys::signal::Signal;
use std::{
    collections::VecDeque,
    future,
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

#[derive(Debug)]
/// Required exec session data.
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
    /// started.
    pub(super) async fn start_exec(session: ExecSession) -> Result<StartedExec> {
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
        let mut args = args
            .exec_sync_args_without_command()
            .context("exec sync args without command")?;
        args.extend(session.command);

        // Read while the runtime runs, see `ContainerIO::collect_stdio`.
        container_io
            .collect_stdio(session.stdout, session.stderr)
            .context("collect exec output")?;

        let res = session
            .child_reaper
            .create_child(
                RuntimeCall {
                    stdin: session.stdin,
                    // The process has no timeout, a runtime which hangs
                    // still gets killed.
                    deadline: Server::exec_runtime_deadline(None),
                    ..RuntimeCall::new(session.server_config.runtime(), &args, pidfile.path())
                },
                &mut container_io,
            )
            .await;
        let (grandchild_pid, token, call) = match res {
            Ok(res) => res,
            Err(e) => {
                let e = container_io.runtime_error(e).await;
                return Err(e.context("create new child process"));
            }
        };
        // The runtime wrote the PID, the file served its purpose.
        drop(pidfile);
        // Not reaped before the registration below, so the PID is still its
        // own.
        let process = Process::open(grandchild_pid);
        let early_stdio = container_io.take_early_stdio().await;

        let io = SharedContainerIO::new(container_io);
        let child = Child {
            id: session.container_id,
            kind: ChildKind::Exec,
            pid: grandchild_pid,
            exit_paths: vec![],
            oom_exit_paths: vec![],
            timeout: None,
            io: io.clone(),
            cleanup_cmd: vec![],
            token: token.clone(),
        };
        // Also if the registration fails, its watcher reports the exit.
        let attach = io.attach().await;
        let stop_output = attach.stop_output_on_exit(token.clone());
        let exit_rx = match session.child_reaper.watch_grandchild(child, vec![], call) {
            Ok(exit_rx) => exit_rx,
            Err(e) => {
                // No session ends the process then, nothing may wait for it.
                if let Err(e) = process.signal_group(Signal::SIGKILL) {
                    debug!("Not killing exec process {grandchild_pid}, it is gone: {e}");
                }
                return Err(e.context("watch grandchild for pid"));
            }
        };

        let mut teardown = ExecTeardown {
            stdio: None,
            _stop_output: stop_output,
            attach,
            token,
            process,
            kill: Arc::default(),
        };
        teardown.stdio = Some(
            io.stdio()
                .await
                .context("retrieve stdout and stderr channels")?,
        );
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
        let (stdout_rx, stderr_rx) = teardown.stdio.clone().context("no stdio channels")?;
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
        // leave a gap, so the stream ends with it.
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
                    InputEvent::Resize(data) => input.resize(&data).await,
                    InputEvent::Ping => sender.ping().await?,
                    // Close the process stdin as well and keep forwarding the
                    // output until it exits.
                    InputEvent::StdinClosed => input.close_container_stdin().await,
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
                    *status = Some(Self::exit_status(res));
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
    /// once, and only goes away without it if it got aborted, like by a
    /// panic, the session must not wait for the exit forever then.
    fn exit_status(res: Result<ExitChannelData, RecvError>) -> Status {
        match res {
            Ok(exit_data) => Status::exited(exit_data.exit_code),
            Err(e) => Status::Error(format!("unable to get the exit of the command: {e}")),
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
/// only ends before the exit of the process if the connection is gone or
/// the exit got lost.
pub(super) struct ExecTeardown {
    /// The stdout and stderr channels of the process.
    stdio: Option<(IOReceiver<IOMessage>, IOReceiver<IOMessage>)>,
    attach: SharedContainerAttach,

    /// Cancelled once the process exited.
    token: CancellationToken,

    /// The process, which its watcher may reap before the token got
    /// cancelled, so it does not get signalled by its PID.
    process: Process,

    /// Set once the session ended with the idle timeout or lost the exit,
    /// which kills the process, like the reaper kills one after its timeout.
    /// Without a session the process would run on with nothing waiting for
    /// it.
    pub(super) kill: Arc<AtomicBool>,

    /// Stops the read loops once the session took the output, after the
    /// exit, dropped after the rest.
    _stop_output: StopOutput,
}

impl Drop for ExecTeardown {
    fn drop(&mut self) {
        // Nothing forwards the output anymore, the read loops discard it
        // instead of waiting for this session.
        if let Some((stdout_rx, stderr_rx)) = &self.stdio {
            stdout_rx.close();
            stderr_rx.close();
        }

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
            return;
        }
        // Without a client the process would wait for stdin forever. Close
        // it, like the end of the session would for a process started by the
        // client itself. A terminal cannot be closed without ending its
        // output, hang it up instead, like a closed connection does.
        if !self.attach.close_container_stdin() {
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
mod tests {
    use super::*;
    use crate::{
        child_reaper::ChildReaper,
        config::Config,
        conmon_capnp::conmon::CgroupManager,
        streaming_server::{ADDR, ExecSession, StreamingServer, testing::*},
    };
    use anyhow::{Context, Result};
    use bytes::Bytes;

    use crate::{
        process::child_pid,
        remotecommand::{ExpectedStreams, SpdyStreams, Version},
        spdy::{Connection, testing::Client},
        streaming_server::session::ClientRx,
    };
    use crate::{remotecommand::STREAM_ERR_BYTE, web};
    use std::{
        os::unix::{fs::PermissionsExt, process::CommandExt, process::ExitStatusExt},
        path::Path,
        sync::Arc,
    };
    use tokio::{
        io::AsyncWriteExt,
        net::TcpStream,
        task::LocalSet,
        time::{self, Duration},
    };

    /// A runtime which takes `delay` to start the process, which then
    /// writes its output, and its config, none without the config of a
    /// test.
    fn slow_runtime(dir: &Path, delay: &str) -> Result<Option<Config>> {
        let runtime = dir.join("runtime");
        std::fs::write(
            &runtime,
            format!(
                "#!/bin/sh\n\
                 sleep {delay}\n\
                 for arg; do case $arg in --pid-file=*) pidfile=${{arg#--pid-file=}};; esac; done\n\
                 sh -c 'echo started' &\n\
                 echo $! >\"$pidfile\"\n"
            ),
        )?;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))?;
        Ok(Config::parse_for_test([
            "--runtime",
            runtime.to_str().context("runtime path")?,
            "--runtime-dir",
            dir.to_str().context("runtime dir")?,
        ]))
    }

    /// An exec session over a WebSocket with the runtime of the config.
    async fn exec_websocket(
        config: Config,
        idle_timeout: Option<Duration>,
    ) -> Result<(StreamingServer, TcpStream)> {
        let mut server = StreamingServer::default();
        server.start_if_required().await?;
        let url = server
            .exec_url(ExecSession {
                child_reaper: Arc::new(ChildReaper::default()),
                tty: false,
                server_config: Arc::new(config),
                cgroup_manager: CgroupManager::Cgroupfs,
                container_id: "id".into(),
                command: vec!["true".into()],
                stdin: false,
                stdout: true,
                stderr: false,
                idle_timeout,
            })
            .await?;
        let path = url
            .strip_prefix(&format!("http://{ADDR}:{}", server.port))
            .context("url")?;
        let mut stream = TcpStream::connect((ADDR, server.port)).await?;
        stream
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: {ADDR}\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                     Sec-WebSocket-Protocol: {PROTOCOL_V5}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
        let response = response_head(&mut stream).await?;
        assert!(response.starts_with("http/1.1 101"), "{response}");
        Ok((server, stream))
    }

    #[tokio::test]
    async fn slow_exec_starts_are_no_idleness() -> Result<()> {
        // The runtime takes longer than the idle timeout.
        let dir = tempfile::tempdir()?;
        let Some(config) = slow_runtime(dir.path(), "1")? else {
            return Ok(());
        };

        LocalSet::new()
            .run_until(async {
                let (_server, mut stream) =
                    exec_websocket(config, Some(Duration::from_millis(500))).await?;

                // The output arrives, the wait for the runtime did not end
                // the session.
                let mut output = vec![];
                loop {
                    match server_frame(&mut stream).await? {
                        (0x8, _) => break,
                        (_, payload) => output.extend(payload),
                    }
                }
                let output = String::from_utf8_lossy(&output);
                assert!(output.contains("started"), "{output}");
                assert!(!output.contains("idle timeout"), "{output}");
                answer_close(&mut stream).await
            })
            .await
    }

    #[tokio::test]
    async fn too_large_messages_fail_slow_exec_starts() -> Result<()> {
        // The runtime takes longer than the session waits for its end.
        let dir = tempfile::tempdir()?;
        let Some(config) = slow_runtime(dir.path(), "4")? else {
            return Ok(());
        };

        LocalSet::new()
            .run_until(async {
                let (_server, stream) = exec_websocket(config, None).await?;
                let (mut reader, writer) = send_too_large(stream);

                // The status and the close come without the runtime.
                let (opcode, status) =
                    time::timeout(web::CLOSE_TIMEOUT / 2, server_frame(&mut reader)).await??;
                assert_eq!((opcode, status.first()), (2, Some(&STREAM_ERR_BYTE)));
                let status = String::from_utf8_lossy(&status[1..]);
                assert!(status.contains(MESSAGE_TOO_LARGE), "{status}");
                too_large_close(reader, writer).await
            })
            .await
    }

    #[tokio::test]
    async fn lost_exit_ends_the_session_and_kills_the_process() -> Result<()> {
        // The output goes to a SPDY client.
        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut connection =
            Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
        let mut client = Client::new(client)?;
        client.open(1, [("streamType", "error")].into()).await?;
        client.open(3, [("streamType", "stdout")].into()).await?;
        let expected = ExpectedStreams {
            stdin: false,
            stdout: true,
            stderr: false,
        };
        let streams = SpdyStreams::wait(&mut connection, expected).await?;
        let (_input, output) = streams.into_parts(connection, Version::V4).await;
        let mut output = Output::spdy(output);

        let mut process = std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()?;
        let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
        // Like a watcher which got aborted before it sent the exit.
        let exit_rx = broadcast::channel(1).1;
        let attach = io.attach().await;
        let stopped = attach.output_stopped().clone();
        let token = CancellationToken::new();
        let teardown = ExecTeardown {
            stdio: Some(io.stdio().await?),
            _stop_output: attach.stop_output_on_exit(token.clone()),
            attach,
            token,
            process: Process::open(child_pid(process.id())?),
            kill: Arc::default(),
        };
        let started = StartedExec {
            io,
            early_stdio: EarlyStdio::default(),
            exit_rx,
            stdin_enabled: false,
            stdout_enabled: true,
            stderr_enabled: false,
            teardown,
        };
        let (client_rx, _senders) = ClientRx::idle();
        let mut status = None;
        time::timeout(
            Duration::from_secs(5),
            StreamingServer::exec_loop(started, &mut output, client_rx, &mut status),
        )
        .await??;
        assert!(
            matches!(&status, Some(Status::Error(e)) if e.contains("unable to get the exit")),
            "{status:?}"
        );
        let exit = tokio::task::spawn_blocking(move || process.wait()).await??;
        assert_eq!(exit.signal(), Some(Signal::SIGKILL as i32));
        // Nothing waits for the read loops, which a lost exit never stops.
        assert!(stopped.is_cancelled());
        Ok(())
    }

    #[test]
    #[expect(clippy::cast_possible_truncation, reason = "a wrapping byte pattern")]
    fn early_output_gets_split_into_frames() {
        let data: Vec<u8> = (0..2 * EARLY_FRAME_SIZE + 10).map(|i| i as u8).collect();
        let frames = StreamingServer::early_frames(data.clone());
        assert_eq!(
            frames.iter().map(Bytes::len).collect::<Vec<_>>(),
            [EARLY_FRAME_SIZE, EARLY_FRAME_SIZE, 10]
        );
        assert_eq!(frames.iter().flatten().copied().collect::<Vec<_>>(), data);
        assert!(StreamingServer::early_frames(vec![]).is_empty());
    }
}
