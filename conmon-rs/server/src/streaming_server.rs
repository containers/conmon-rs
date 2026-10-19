use crate::{
    attach::SharedContainerAttach,
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
use conmon_common::conmon_capnp::conmon::CgroupManager;
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
use tracing::{debug, debug_span, error, info, trace, warn};
use uuid::Uuid;

const ADDR: &str = "127.0.0.1";

const PROTOCOL_V5: &str = "v5.channel.k8s.io";
const PROTOCOL_PORT_FORWARD: &str = "SPDY/3.1+portforward.k8s.io";

const EXEC_PATH: &str = "exec";
const ATTACH_PATH: &str = "attach";
const PORT_FORWARD_PATH: &str = "port-forward";

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
    PortForward(PortForwardSession),
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
}

#[derive(Debug)]
/// Required port forward session data.
struct PortForwardSession {
    #[allow(dead_code)]
    net_ns_path: Box<str>,
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
            .route(&Self::path_for(PORT_FORWARD_PATH), get(Self::handle))
            // for allowing proper error handling on wrong protocol usage (spdy instead of websocket)
            .route(&Self::path_for(EXEC_PATH), post(Self::handle))
            .route(&Self::path_for(ATTACH_PATH), post(Self::handle))
            .route(&Self::path_for(PORT_FORWARD_PATH), post(Self::handle))
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
            }),
        );
        self.url_for(ATTACH_PATH, &uuid)
    }

    /// Returns the URL used for the provided port forward parameters.
    pub async fn port_forward_url(&self, net_ns_path: String) -> String {
        let mut state_lock = self.state.write().await;
        let uuid = Uuid::new_v4();
        state_lock.insert(
            uuid,
            Session::PortForward(PortForwardSession {
                net_ns_path: net_ns_path.into_boxed_str(),
            }),
        );
        self.url_for(PORT_FORWARD_PATH, &uuid)
    }

    /// Handle a webserver connection which should be upgraded to become a websocket one.
    async fn handle(
        ws: WebSocketUpgrade,
        Path(token): Path<Uuid>,
        AxumState(state): AxumState<Arc<RwLock<State>>>,
    ) -> impl IntoResponse {
        let span = debug_span!("handle_common", %token);
        let _enter = span.enter();

        info!("Got request for token: {token}");
        let mut state_lock = state.write().await;

        match state_lock.remove(&token) {
            Some(session) => {
                info!("Got valid session for token {token}: {session:?}");
                ws.protocols([PROTOCOL_V5, PROTOCOL_PORT_FORWARD])
                    .on_upgrade(move |socket| Self::handle_websocket(socket, session))
            }
            None => {
                error!("Unable to find session for token: {token}");
                StatusCode::NOT_FOUND.into_response()
            }
        }
    }

    /// Handle a single websocket connection.
    async fn handle_websocket(socket: WebSocket, session: Session) {
        let (sender, receiver) = socket.split();
        let (stdin_tx, stdin_rx) = mpsc::channel(16);

        let mut send_task = Self::write_task(sender, stdin_rx, session).await;
        let mut recv_task = Self::read_task(receiver, stdin_tx).await;

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
    async fn write_task(
        mut sender: SplitSink<WebSocket, Message>,
        stdin_rx: MpscReceiver<Bytes>,
        session: Session,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let (Err(e), typ) = match session {
                Session::Exec(s) => (Self::exec_loop(*s, &mut sender, stdin_rx).await, "exec"),
                Session::Attach(s) => (Self::attach_loop(s, &mut sender, stdin_rx).await, "attach"),
                Session::PortForward(s) => (
                    Self::port_forward_loop(s, &mut sender, stdin_rx).await,
                    "port forward",
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
    async fn read_task(
        mut receiver: SplitStream<WebSocket>,
        stdin_tx: MpscSender<Bytes>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(Ok(msg)) = receiver.next().await {
                if Self::read_message(msg, &stdin_tx).await.is_break() {
                    break;
                }
            }
        })
    }

    /// Read a single message and return the control flow decision.
    async fn read_message(msg: Message, stdin_tx: &MpscSender<Bytes>) -> ControlFlow<(), ()> {
        match msg {
            Message::Binary(data) if !data.is_empty() => {
                debug!("Got {} binary bytes", data.len());
                if let Err(e) = stdin_tx.send(data).await {
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
    ) -> Result<()> {
        let pidfile = ContainerIO::temp_file_name(
            Some(session.server_config.runtime_dir()),
            "exec_streaming",
            "pid",
        );

        let args = GenerateRuntimeArgs {
            config: &session.server_config,
            id: &session.container_id,
            container_io: &session.container_io,
            pidfile: &pidfile,
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
                &pidfile,
                vec![],
                vec![],
            )
            .await
            .context("create new child process")?;

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

        let child = Child::new(
            container_id,
            ChildKind::Exec,
            grandchild_pid,
            vec![],
            vec![],
            None,
            io.clone(),
            vec![],
            token.clone(),
        );

        let mut exit_rx = child_reaper
            .watch_grandchild(child, vec![])
            .context("watch grandchild for pid")?;

        let (stdout_rx, stderr_rx) = io
            .stdio()
            .await
            .context("retrieve stdout and stderr channels")?;

        let attach = io.attach().await;

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

                Some(data) = stdin_rx.recv() => if stdin_enabled
                    && Self::handle_stdin(data, &attach, &io).await?.is_break()
                {
                    break
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
    ) -> Result<()> {
        // Extract values we need for the I/O loop
        let stdin_enabled = session.stdin;
        let stdout_enabled = session.stdout;
        let stderr_enabled = session.stderr;
        let io = session.child.io().clone();
        let token = session.child.token().clone();

        // Explicitly drop session to free the ReapableChild before entering the I/O loop
        drop(session);

        // Subscribe to the attach broadcast rather than the stdio channels. The
        // broadcast only carries output produced while someone is attached,
        // whereas the stdio channels replay buffered history and are closed
        // for created containers anyway.
        let attach = io.attach().await;
        let mut attach_rx = attach.subscribe();

        // Every read loop sends one `Done` once its pipe ended. Count them to
        // know when the output is complete.
        let expected_done = io.pipe_count().await;
        let mut done = 0;

        loop {
            tokio::select! {
                res = attach_rx.recv(), if done < expected_done => match res {
                    Ok(IOMessage::Done) => done += 1,
                    Ok(message) => {
                        Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
                    }
                    Err(RecvError::Lagged(n)) => warn!("Attach client too slow, skipped {n} messages"),
                    Err(RecvError::Closed) => {
                        debug!("Exiting streaming attach because container I/O is gone");
                        break
                    }
                },

                Some(data) = stdin_rx.recv() => if stdin_enabled
                    && Self::handle_stdin(data, &attach, &io).await?.is_break()
                {
                    break
                },

                _ = token.cancelled() => {
                    debug!("Exiting streaming attach because token cancelled");
                    if done < expected_done {
                        Self::drain_attach(&mut attach_rx, expected_done - done, stdout_enabled, stderr_enabled, sender).await?;
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
    /// container exited, until the remaining read loops signalled the end or
    /// the [`Drain`] gives up.
    async fn drain_attach(
        attach_rx: &mut broadcast::Receiver<IOMessage>,
        mut remaining_done: usize,
        stdout_enabled: bool,
        stderr_enabled: bool,
        sender: &mut SplitSink<WebSocket, Message>,
    ) -> Result<()> {
        let mut drain = Drain::default();
        while remaining_done > 0 {
            match drain.recv(attach_rx.recv()).await {
                Some(Ok(IOMessage::Done)) => remaining_done -= 1,
                Some(Ok(message)) => {
                    Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
                }
                Some(Err(RecvError::Lagged(n))) => {
                    warn!("Attach client too slow, skipped {n} messages");
                }
                Some(Err(RecvError::Closed)) | None => return Ok(()),
            }
        }
        Ok(())
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

    /// The port forward specific read/write loop.
    async fn port_forward_loop(
        _session: PortForwardSession,
        _sender: &mut SplitSink<WebSocket, Message>,
        mut _in_rx: MpscReceiver<Bytes>,
    ) -> Result<()> {
        todo!("Requires SPDY protocol implementation from https://github.com/moby/spdystream")
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
    /// byte of the message selects the stream, the rest is the payload.
    async fn handle_stdin(
        data: Bytes,
        attach: &SharedContainerAttach,
        io: &SharedContainerIO,
    ) -> Result<ControlFlow<()>> {
        let Some(&msg_type) = data.first() else {
            return Ok(ControlFlow::Continue(()));
        };
        // Slicing the reference counted bytes avoids copying the payload.
        let payload = data.slice(1..);

        match msg_type {
            STDIN_BYTE => {
                trace!("Got stdin message of len {}", payload.len());
                attach
                    .stdin()
                    .send(payload)
                    .context("send to attach session")?;
            }
            RESIZE_BYTE => {
                let e = serde_json::from_slice::<ResizeEvent>(&payload)
                    .context("unmarshal resize event")?;
                trace!("Got resize message: {e:?}");
                io.resize(e.width, e.height)
                    .await
                    .context("resize terminal")?;
            }
            CLOSE_BYTE => {
                info!("Got close message");
                return Ok(ControlFlow::Break(()));
            }
            x => warn!("Unknown start byte for stdin: {x}"),
        }

        Ok(ControlFlow::Continue(()))
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
    async fn drain_stops_on_quiet_channel() {
        let (_tx, rx) = async_channel::bounded::<()>(1);
        let start = Instant::now();
        assert!(Drain::default().recv(rx.recv()).await.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
