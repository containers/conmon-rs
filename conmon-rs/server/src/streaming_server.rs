use crate::conmon_capnp::conmon::CgroupManager;
use crate::{
    bounded_hashmap::BoundedHashMap,
    child::{Child, ChildKind},
    child_reaper::{ChildReaper, ReapableChild},
    config::Config,
    container_io::{ContainerIO, Message as IOMessage, Pipe, SharedContainerIO},
    server::GenerateRuntimeArgs,
};
use anyhow::{Context, Result};
use async_channel::Receiver as IOReceiver;
use axum::{
    Router,
    extract::{
        Path, State as AxumState,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code},
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use bytes::{BufMut, Bytes, BytesMut};
use futures::{
    sink::SinkExt,
    stream::{SplitSink, SplitStream, StreamExt},
};
use serde::{Deserialize, Serialize};
use std::{fmt::Debug, ops::ControlFlow, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{
        RwLock,
        broadcast::{self, error::RecvError},
        mpsc::{self, Receiver as MpscReceiver, Sender as MpscSender},
    },
    task::{self, JoinHandle},
    time::timeout,
};
use tower_http::trace::TraceLayer;
use tracing::{Instrument, debug, debug_span, error, info, trace, warn};
use uuid::Uuid;

const ADDR: &str = "127.0.0.1";

const PROTOCOL_V5: &str = "v5.channel.k8s.io";

const EXEC_PATH: &str = "exec";
const ATTACH_PATH: &str = "attach";

const STDIN_BYTE: u8 = 0;
const STDOUT_BYTE: u8 = 1;
const STDERR_BYTE: u8 = 2;
const STREAM_ERR_BYTE: u8 = 3;
const RESIZE_BYTE: u8 = 4;
const CLOSE_BYTE: u8 = 255;

/// Upper bound for waiting on the next output after the container exited.
/// The read loops may still be draining the pipes when the exit is observed,
/// they signal the end with `Message::Done`. The timeout only kicks in if that
/// never arrives, for example because a grandchild keeps the pipe open.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

/// Upper bound for the messages forwarded per drain after the container
/// exited. The output written before the exit is at most what the channels
/// (100 messages) and the pipe buffers hold. With the default pipe-max-size
/// of 1 MiB in 4 KiB reads, that is 256 messages per pipe, so this is well
/// above. It is a heuristic though, a process with CAP_SYS_RESOURCE can grow
/// its pipes further. More output stems from a grandchild which keeps the
/// pipe open and writing, which would otherwise keep the session open
/// forever. Unlike a time limit this does not cut off the output for a slow
/// client.
const DRAIN_MAX_MESSAGES: usize = 1024;

#[derive(Debug, Default)]
/// The main streaming server structure of this module.
pub struct StreamingServer {
    running: bool,
    port: u16,
    state: Arc<RwLock<State>>,
}

/// State handled by the streaming server.
type State = BoundedHashMap<Uuid, Session>;

#[derive(Debug)]
/// A dedicated session for each provided functionality.
enum Session {
    Exec(Box<ExecSession>),
    Attach(AttachSession),
}

#[derive(Debug)]
/// Required exec session data.
struct ExecSession {
    child_reaper: Arc<ChildReaper>,
    container_io: ContainerIO,
    server_config: Arc<Config>,
    cgroup_manager: CgroupManager,
    container_id: Box<str>,
    command: Vec<String>,
    stdin: bool,
    stdout: bool,
    stderr: bool,
}

#[derive(Debug)]
/// Required attach session data.
struct AttachSession {
    child: ReapableChild,
    stdin: bool,
    stdout: bool,
    stderr: bool,
    stop_after_stdin_eof: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
/// Terminal resize event for exec and attach.
struct ResizeEvent {
    width: u16,
    height: u16,
}

#[derive(Debug, Serialize)]
/// Error message type used for exec in case that the command fails.
struct ErrorMessage {
    status: &'static str,
    reason: &'static str,
    details: ErrorDetails,
    message: String,
}

impl ErrorMessage {
    fn new<T>(exit_code: T) -> Self
    where
        T: ToString,
    {
        Self {
            status: "Failure",
            reason: "NonZeroExitCode",
            details: ErrorDetails {
                causes: vec![ErrorCause {
                    reason: "ExitCode",
                    message: exit_code.to_string(),
                }],
            },
            message: "command terminated with non-zero exit code".into(),
        }
    }

    /// An error of the session itself, for example a command which could not
    /// be started.
    fn internal(message: String) -> Self {
        Self {
            status: "Failure",
            reason: "InternalError",
            details: ErrorDetails { causes: vec![] },
            message,
        }
    }
}

#[derive(Debug, Serialize)]
/// Error details for the ErrorMessage.
struct ErrorDetails {
    causes: Vec<ErrorCause>,
}

#[derive(Debug, Serialize)]
/// Error cause for the ErrorDetails.
struct ErrorCause {
    reason: &'static str,
    message: String,
}

impl StreamingServer {
    /// Start the streaming server if not already running.
    pub async fn start_if_required(&mut self) -> Result<()> {
        if self.running {
            return Ok(());
        }

        let listener = TcpListener::bind(ADDR.to_string() + ":0")
            .await
            .context("bind streaming server")?;

        let local_addr = listener
            .local_addr()
            .context("get listeners local address")?;

        self.port = local_addr.port();

        info!("Starting streaming server on {local_addr}");
        task::spawn_local(Self::serve(listener, self.state.clone()));
        self.running = true;

        Ok(())
    }

    /// Serve the main streaming server.
    async fn serve(listener: TcpListener, state: Arc<RwLock<State>>) -> Result<()> {
        let router = Router::new()
            .route(&Self::path_for(EXEC_PATH), get(Self::handle))
            .route(&Self::path_for(ATTACH_PATH), get(Self::handle))
            // for allowing proper error handling on wrong protocol usage (spdy instead of websocket)
            .route(&Self::path_for(EXEC_PATH), post(Self::handle))
            .route(&Self::path_for(ATTACH_PATH), post(Self::handle))
            .fallback(Self::fallback)
            .with_state(state)
            .layer(TraceLayer::new_for_http());
        axum::serve(listener, router)
            .await
            .context("start streaming server")
    }

    /// Token parse path for the web server.
    fn path_for(p: &str) -> String {
        format!("/{p}/") + "{token}"
    }

    /// Return the URL for a specific path and Uuid.
    fn url_for(&self, p: &str, uuid: &Uuid) -> String {
        format!("http://{ADDR}:{0}/{p}/{uuid}", self.port)
    }

    /// Fallback response.
    async fn fallback() -> impl IntoResponse {
        StatusCode::NOT_FOUND
    }

    #[allow(clippy::too_many_arguments)]
    /// Returns the URL used for the provided exec parameters.
    pub async fn exec_url(
        &self,
        child_reaper: Arc<ChildReaper>,
        container_io: ContainerIO,
        server_config: Arc<Config>,
        cgroup_manager: CgroupManager,
        container_id: Box<str>,
        command: Vec<String>,
        stdin: bool,
        stdout: bool,
        stderr: bool,
    ) -> String {
        let mut state_lock = self.state.write().await;
        let uuid = Uuid::new_v4();
        state_lock.insert(
            uuid,
            Session::Exec(
                ExecSession {
                    child_reaper,
                    container_io,
                    server_config,
                    cgroup_manager,
                    container_id,
                    command,
                    stdin,
                    stdout,
                    stderr,
                }
                .into(),
            ),
        );
        self.url_for(EXEC_PATH, &uuid)
    }

    /// Returns the URL used for the provided attach parameters.
    pub async fn attach_url(
        &self,
        child: ReapableChild,
        stdin: bool,
        stdout: bool,
        stderr: bool,
        stop_after_stdin_eof: bool,
    ) -> String {
        let mut state_lock = self.state.write().await;
        let uuid = Uuid::new_v4();
        state_lock.insert(
            uuid,
            Session::Attach(AttachSession {
                child,
                stdin,
                stdout,
                stderr,
                stop_after_stdin_eof,
            }),
        );
        self.url_for(ATTACH_PATH, &uuid)
    }

    /// Handle a webserver connection which should be upgraded to become a websocket one.
    async fn handle(
        ws: WebSocketUpgrade,
        Path(token): Path<Uuid>,
        AxumState(state): AxumState<Arc<RwLock<State>>>,
    ) -> impl IntoResponse {
        async move {
            info!("Got request for token: {token}");
            let session = state.write().await.remove(&token);

            match session {
                Some(session) => {
                    info!("Got valid session for token {token}: {session:?}");
                    ws.protocols([PROTOCOL_V5])
                        .on_upgrade(move |socket| Self::handle_websocket(socket, session))
                }
                None => {
                    error!("Unable to find session for token: {token}");
                    StatusCode::NOT_FOUND.into_response()
                }
            }
        }
        .instrument(debug_span!("handle_common", %token))
        .await
    }

    /// Handle a single websocket connection.
    async fn handle_websocket(socket: WebSocket, session: Session) {
        let (sender, receiver) = socket.split();
        let (stdin_tx, stdin_rx) = mpsc::channel(16);
        // Resize events go through their own channel, so that they do not
        // queue behind stdin the container does not read.
        let (resize_tx, resize_rx) = mpsc::channel(16);

        let mut send_task = Self::write_task(sender, stdin_rx, resize_rx, session);
        let mut recv_task = Self::read_task(receiver, stdin_tx, resize_tx);

        tokio::select! {
            rv_a = (&mut send_task) => {
                match rv_a {
                    Ok(_) => info!("All messages sent"),
                    Err(a) => error!("Error sending messages: {a:?}")
                }
                recv_task.abort();
            },
            rv_b = (&mut recv_task) => {
                match rv_b {
                    Ok(_) => info!("All messages received"),
                    Err(b) => error!("Error receiving messages: {b:?}")
                }
                send_task.abort();
            }
        }

        info!("Closing websocket connection");
    }

    /// Build a common write task based on the session type.
    fn write_task(
        mut sender: SplitSink<WebSocket, Message>,
        stdin_rx: MpscReceiver<Bytes>,
        resize_rx: MpscReceiver<Bytes>,
        session: Session,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let (Err(e), typ) = match session {
                Session::Exec(s) => (
                    Self::exec_loop(*s, &mut sender, stdin_rx, resize_rx).await,
                    "exec",
                ),
                Session::Attach(s) => (
                    Self::attach_loop(s, &mut sender, stdin_rx, resize_rx).await,
                    "attach",
                ),
            } {
                error!("Unable to run {typ} for container: {e:#}");

                // Otherwise the client sees a clean close and assumes success.
                if let Err(e) = Self::send_error(&mut sender, format!("{typ} failed: {e:#}")).await
                {
                    error!("Unable to send error message: {e:#}");
                }
            }

            if let Err(e) = sender
                .send(Message::Close(
                    CloseFrame {
                        code: close_code::NORMAL,
                        reason: "done".into(),
                    }
                    .into(),
                ))
                .await
            {
                error!("Unable to send close message: {e}")
            }
        })
    }

    /// Build a common read task.
    fn read_task(
        mut receiver: SplitStream<WebSocket>,
        stdin_tx: MpscSender<Bytes>,
        resize_tx: MpscSender<Bytes>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(Ok(msg)) = receiver.next().await {
                if Self::read_message(msg, &stdin_tx, &resize_tx)
                    .await
                    .is_break()
                {
                    break;
                }
            }
        })
    }

    /// Read a single message and return the control flow decision.
    async fn read_message(
        msg: Message,
        stdin_tx: &MpscSender<Bytes>,
        resize_tx: &MpscSender<Bytes>,
    ) -> ControlFlow<(), ()> {
        match msg {
            Message::Binary(data) if !data.is_empty() => {
                debug!("Got {} binary bytes", data.len());
                let tx = if data[0] == RESIZE_BYTE {
                    resize_tx
                } else {
                    stdin_tx
                };
                if let Err(e) = tx.send(data).await {
                    error!("Unable to send stdin data: {e}");
                }
            }
            Message::Close(c) => {
                if let Some(cf) = c {
                    info!(
                        "Got websocket close with code {} and reason `{}`",
                        cf.code, cf.reason
                    );
                } else {
                    warn!("Got close message without close frame");
                }
                return ControlFlow::Break(());
            }
            Message::Text(t) => trace!("Got text message: {t:?}"),
            Message::Pong(_) => trace!("Got pong"),
            Message::Ping(_) => trace!("Got ping"),
            Message::Binary(_) => trace!("Got unknown binary data"),
        }

        ControlFlow::Continue(())
    }

    /// The exec specific read/write loop.
    async fn exec_loop(
        mut session: ExecSession,
        sender: &mut SplitSink<WebSocket, Message>,
        mut stdin_rx: MpscReceiver<Bytes>,
        mut resize_rx: MpscReceiver<Bytes>,
    ) -> Result<()> {
        let pidfile = ContainerIO::temp_file(
            Some(session.server_config.runtime_dir()),
            "exec_streaming",
            "pid",
        );

        let args = GenerateRuntimeArgs {
            config: &session.server_config,
            id: &session.container_id,
            container_io: &session.container_io,
            pidfile: pidfile.path(),
            cgroup_manager: session.cgroup_manager,
        };
        let mut args = args
            .exec_sync_args_without_command()
            .context("exec sync args without command")?;
        args.extend(session.command);

        let (grandchild_pid, token) = session
            .child_reaper
            .create_child(
                session.server_config.runtime(),
                &args,
                session.stdin,
                &mut session.container_io,
                pidfile.path(),
                vec![],
                vec![],
            )
            .await
            .context("create new child process")?;
        // The runtime wrote the PID, the file served its purpose.
        drop(pidfile);

        // Extract all values we need before dropping session to free
        // server_config Arc and cgroup_manager before the I/O loop
        let ExecSession {
            stdin,
            stdout,
            stderr,
            container_id,
            container_io,
            child_reaper,
            server_config: _,
            cgroup_manager: _,
            command: _,
        } = session;

        let stdin_enabled = stdin;
        let stdout_enabled = stdout;
        let stderr_enabled = stderr;
        let io = SharedContainerIO::new(container_io);
        let child_reaper = child_reaper;

        let child = Child {
            id: container_id,
            kind: ChildKind::Exec,
            pid: grandchild_pid,
            exit_paths: vec![],
            oom_exit_paths: vec![],
            timeout: None,
            io: io.clone(),
            cleanup_cmd: vec![],
            token: token.clone(),
        };

        let mut exit_rx = child_reaper
            .watch_grandchild(child, vec![])
            .context("watch grandchild for pid")?;

        let (stdout_rx, stderr_rx) = io
            .stdio()
            .await
            .context("retrieve stdout and stderr channels")?;

        let attach = io.attach().await;
        let mut pending_stdin = None;
        let mut stdin_closed = false;

        // Streams which are disabled or whose read loop signalled the end are
        // not polled anymore.
        let mut stdout_done = !stdout_enabled;
        let mut stderr_done = !stderr_enabled;

        loop {
            tokio::select! {
                res = stdout_rx.recv(), if !stdout_done => match res {
                    Ok(IOMessage::Data(data, _)) => {
                        Self::frame_and_send(STDOUT_BYTE, &data, sender).await
                            .context("send to stdout")?;
                    }
                    Ok(IOMessage::Done) | Err(_) => stdout_done = true,
                },

                res = stderr_rx.recv(), if !stderr_done => match res {
                    Ok(IOMessage::Data(data, _)) => {
                        Self::frame_and_send(STDERR_BYTE, &data, sender).await
                            .context("send to stderr")?;
                    }
                    Ok(IOMessage::Done) | Err(_) => stderr_done = true,
                },

                // Resizing does not depend on stdin, a terminal without it
                // still has a size.
                Some(data) = resize_rx.recv() => {
                    Self::handle_resize(&data, &io).await?;
                },

                // Wait for the container to take the stdin without blocking the
                // output meanwhile.
                () = attach.send_stdin(pending_stdin.clone().unwrap_or_default()), if pending_stdin.is_some() => {
                    pending_stdin = None
                },

                Some(data) = stdin_rx.recv(), if pending_stdin.is_none() => if stdin_enabled && !stdin_closed {
                    match Self::handle_stdin(data) {
                        // The client closed its stdin. Close the process stdin
                        // as well and keep forwarding the output until it
                        // exits, a terminal ends the session instead.
                        ControlFlow::Break(()) if attach.close_container_stdin() => stdin_closed = true,
                        ControlFlow::Break(()) => break,
                        ControlFlow::Continue(stdin) => pending_stdin = stdin,
                    }
                },

                Ok(exit_data) = exit_rx.recv() => {
                    // Forward what the read loops still deliver before closing.
                    if !stdout_done {
                        Self::drain_stdio(&stdout_rx, STDOUT_BYTE, sender).await.context("drain stdout")?;
                    }
                    if !stderr_done {
                        Self::drain_stdio(&stderr_rx, STDERR_BYTE, sender).await.context("drain stderr")?;
                    }
                    if exit_data.exit_code != 0 {
                        let mut err = vec![STREAM_ERR_BYTE];
                        let msg = ErrorMessage::new(exit_data.exit_code);
                        err.extend(serde_json::to_vec(&msg).context("serialize error message")?);
                        sender.send(Message::Binary(err.into())).await.context("send exit failure message")?;
                    }
                    break
                },
            }
        }

        Ok(())
    }

    /// The attach specific read/write loop.
    async fn attach_loop(
        session: AttachSession,
        sender: &mut SplitSink<WebSocket, Message>,
        mut stdin_rx: MpscReceiver<Bytes>,
        mut resize_rx: MpscReceiver<Bytes>,
    ) -> Result<()> {
        // Extract values we need for the I/O loop
        let stdin_enabled = session.stdin;
        let stdout_enabled = session.stdout;
        let stderr_enabled = session.stderr;
        let stop_after_stdin_eof = session.stop_after_stdin_eof;
        let io = session.child.io().clone();
        let token = session.child.token().clone();

        // Explicitly drop session to free the ReapableChild before entering the I/O loop
        drop(session);

        // Subscribe to the attach broadcast rather than the stdio channels. The
        // broadcast only carries output produced while someone is attached,
        // whereas the stdio channels replay buffered history and are closed
        // for created containers anyway.
        let attach = io.attach().await;
        let mut pending_stdin = None;
        let mut stdin_closed = false;

        // A single `Done` follows once all pipes ended. It may have been sent
        // before subscribing, then only forward what is still queued.
        let (mut attach_rx, mut done) = attach.subscribe();
        if done {
            while let Ok(message) = attach_rx.try_recv() {
                Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
            }
        }

        loop {
            tokio::select! {
                res = attach_rx.recv(), if !done => match res {
                    Ok(IOMessage::Done) => {
                        done = true;
                        // Nothing is left to forward in either direction.
                        if stdin_closed {
                            break
                        }
                    }
                    Ok(message) => {
                        Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
                    }
                    Err(RecvError::Lagged(n)) => warn!("Attach client too slow, skipped {n} messages"),
                    Err(RecvError::Closed) => {
                        debug!("Exiting streaming attach because container I/O is gone");
                        break
                    }
                },

                // Resizing does not depend on stdin, a terminal without it
                // still has a size.
                Some(data) = resize_rx.recv() => {
                    Self::handle_resize(&data, &io).await?;
                },

                // Wait for the container to take the stdin without blocking the
                // output meanwhile.
                () = attach.send_stdin(pending_stdin.clone().unwrap_or_default()), if pending_stdin.is_some() => {
                    pending_stdin = None
                },

                Some(data) = stdin_rx.recv(), if pending_stdin.is_none() => if stdin_enabled && !stdin_closed {
                    match Self::handle_stdin(data) {
                        // The client closed its stdin. With stdin once, close
                        // the container stdin as well and keep forwarding the
                        // output until it ends.
                        ControlFlow::Break(()) if !done && stop_after_stdin_eof && attach.close_container_stdin() => {
                            stdin_closed = true
                        }
                        ControlFlow::Break(()) => break,
                        ControlFlow::Continue(stdin) => pending_stdin = stdin,
                    }
                },

                _ = token.cancelled() => {
                    debug!("Exiting streaming attach because token cancelled");
                    if !done {
                        Self::drain_attach(&mut attach_rx, stdout_enabled, stderr_enabled, sender).await?;
                    }
                    break
                }
            }
        }

        Ok(())
    }

    /// Forward a single output message to the WebSocket client.
    async fn forward_output(
        message: IOMessage,
        stdout_enabled: bool,
        stderr_enabled: bool,
        sender: &mut SplitSink<WebSocket, Message>,
    ) -> Result<()> {
        match message {
            IOMessage::Data(data, Pipe::StdOut) if stdout_enabled => {
                Self::frame_and_send(STDOUT_BYTE, &data, sender)
                    .await
                    .context("send to stdout")
            }
            IOMessage::Data(data, Pipe::StdErr) if stderr_enabled => {
                Self::frame_and_send(STDERR_BYTE, &data, sender)
                    .await
                    .context("send to stderr")
            }
            IOMessage::Data(..) | IOMessage::Done => Ok(()),
        }
    }

    /// Forward the output which is still queued or arrives shortly after the
    /// container exited, until the output ended or the [`Drain`] gives up.
    async fn drain_attach(
        attach_rx: &mut broadcast::Receiver<IOMessage>,
        stdout_enabled: bool,
        stderr_enabled: bool,
        sender: &mut SplitSink<WebSocket, Message>,
    ) -> Result<()> {
        let mut drain = Drain::default();
        loop {
            match drain.recv(attach_rx.recv()).await {
                Some(Ok(IOMessage::Done)) => return Ok(()),
                Some(Ok(message)) => {
                    Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
                }
                Some(Err(RecvError::Lagged(n))) => {
                    warn!("Attach client too slow, skipped {n} messages");
                }
                Some(Err(RecvError::Closed)) | None => return Ok(()),
            }
        }
    }

    /// Forward the remaining output of an exec stdio channel after the process
    /// exited, until the read loop signals that it is done or the [`Drain`]
    /// gives up.
    async fn drain_stdio(
        rx: &IOReceiver<IOMessage>,
        stream_byte: u8,
        sender: &mut SplitSink<WebSocket, Message>,
    ) -> Result<()> {
        let mut drain = Drain::default();
        loop {
            match drain.recv(rx.recv()).await {
                Some(Ok(IOMessage::Data(data, _))) => {
                    Self::frame_and_send(stream_byte, &data, sender).await?;
                }
                Some(Ok(IOMessage::Done)) | Some(Err(_)) | None => return Ok(()),
            }
        }
    }

    /// Send a session error on the error stream.
    async fn send_error(sender: &mut SplitSink<WebSocket, Message>, message: String) -> Result<()> {
        let mut frame = vec![STREAM_ERR_BYTE];
        frame.extend(
            serde_json::to_vec(&ErrorMessage::internal(message))
                .context("serialize error message")?,
        );
        sender
            .send(Message::Binary(frame.into()))
            .await
            .context("send error message")
    }

    /// Handle a single stdin message from the websocket client. The first
    /// byte of the message selects the stream, the rest is the payload. Returns
    /// the payload which has to be sent to the container stdin, if any.
    fn handle_stdin(data: Bytes) -> ControlFlow<(), Option<Bytes>> {
        let Some(&msg_type) = data.first() else {
            return ControlFlow::Continue(None);
        };
        // Slicing the reference counted bytes avoids copying the payload.
        let payload = data.slice(1..);

        match msg_type {
            STDIN_BYTE => {
                trace!("Got stdin message of len {}", payload.len());
                return ControlFlow::Continue(Some(payload));
            }
            CLOSE_BYTE => {
                info!("Got close message");
                return ControlFlow::Break(());
            }
            x => warn!("Unknown start byte for stdin: {x}"),
        }

        ControlFlow::Continue(None)
    }

    /// Handle a single resize message from the websocket client.
    async fn handle_resize(data: &[u8], io: &SharedContainerIO) -> Result<()> {
        let e = serde_json::from_slice::<ResizeEvent>(data.get(1..).unwrap_or_default())
            .context("unmarshal resize event")?;
        trace!("Got resize message: {e:?}");
        // Clients send resize events regardless of a terminal, they are no
        // reason to end the session without one.
        if !io.try_resize(e.width, e.height).await? {
            debug!("Ignoring resize event, container has no terminal");
        }
        Ok(())
    }

    /// Prepend a stream type byte and send the data over WebSocket. The frame
    /// is allocated with its exact size and handed over without copying.
    async fn frame_and_send(
        stream_byte: u8,
        data: &[u8],
        sender: &mut SplitSink<WebSocket, Message>,
    ) -> Result<()> {
        let mut frame = BytesMut::with_capacity(data.len() + 1);
        frame.put_u8(stream_byte);
        frame.extend_from_slice(data);
        sender
            .send(Message::Binary(frame.freeze()))
            .await
            .context("send framed data")
    }
}

/// Bounds forwarding the output after the container exited.
struct Drain {
    remaining: usize,
}

impl Default for Drain {
    fn default() -> Self {
        Self {
            remaining: DRAIN_MAX_MESSAGES,
        }
    }
}

impl Drain {
    /// Wait for the next message, giving up once the channel stays quiet for
    /// `DRAIN_TIMEOUT` or `DRAIN_MAX_MESSAGES` were received.
    async fn recv<F: Future>(&mut self, recv: F) -> Option<F::Output> {
        let Some(remaining) = self.remaining.checked_sub(1) else {
            warn!(
                "Dropping remaining output after forwarding {DRAIN_MAX_MESSAGES} messages since the exit"
            );
            return None;
        };
        self.remaining = remaining;
        timeout(DRAIN_TIMEOUT, recv).await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_log::ContainerLog;
    use tokio::time::Instant;

    #[tokio::test]
    async fn drain_stops_on_busy_channel() {
        let (tx, rx) = async_channel::bounded(16);
        tokio::spawn(async move { while tx.send(()).await.is_ok() {} });

        let mut drain = Drain::default();
        let mut received = 0;
        while drain.recv(rx.recv()).await.is_some() {
            received += 1;
        }
        assert_eq!(received, DRAIN_MAX_MESSAGES);
    }

    #[tokio::test]
    async fn resize_without_terminal_is_ignored() -> Result<()> {
        let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
        let event = br#"{"Width":80,"Height":24}"#;
        let mut data = vec![RESIZE_BYTE];
        data.extend_from_slice(event);
        StreamingServer::handle_resize(&data, &io).await?;

        // Garbage is still an error.
        assert!(
            StreamingServer::handle_resize(&[RESIZE_BYTE, b'x'], &io)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn drain_stops_on_quiet_channel() {
        let (_tx, rx) = async_channel::bounded::<()>(1);
        let start = Instant::now();
        assert!(Drain::default().recv(rx.recv()).await.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
