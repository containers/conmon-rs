use crate::{
    bounded_hashmap::BoundedHashMap,
    child::Child,
    child_reaper::{ChildReaper, ReapableChild},
    config::Config,
    container_io::{ContainerIO, Message as IOMessage, SharedContainerIO},
    server::GenerateRuntimeArgs,
};
use anyhow::{Context, Result};
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
use conmon_common::conmon_capnp::conmon::CgroupManager;
use futures::{
    sink::SinkExt,
    stream::{SplitSink, SplitStream, StreamExt},
};
use serde::{Deserialize, Serialize};
use std::{fmt::Debug, ops::ControlFlow, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::{
        RwLock,
        mpsc::{self, Receiver as MpscReceiver, Sender as MpscSender},
    },
    task::{self, JoinHandle},
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
    container_id: String,
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
    net_ns_path: String,
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
    message: &'static str,
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
            message: "command terminated with non-zero exit code",
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
        container_id: String,
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
            Session::PortForward(PortForwardSession { net_ns_path }),
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
        stdin_rx: MpscReceiver<Vec<u8>>,
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
                error!("Unable to run {typ} for container: {e}");
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
        stdin_tx: MpscSender<Vec<u8>>,
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
    async fn read_message(msg: Message, stdin_tx: &MpscSender<Vec<u8>>) -> ControlFlow<(), ()> {
        match msg {
            Message::Binary(data) if !data.is_empty() => {
                debug!("Got {} binary bytes", data.len());
                if let Err(e) = stdin_tx.send(data.into()).await {
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
        mut stdin_rx: MpscReceiver<Vec<u8>>,
    ) -> Result<()> {
        let pidfile = ContainerIO::temp_file_name(
            Some(session.server_config.runtime_dir()),
            "exec_streaming",
            "pid",
        )
        .context("build pid file path")?;

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

        let container_id = session.container_id;
        let io = SharedContainerIO::new(session.container_io);
        let child = Child::new(
            container_id,
            grandchild_pid,
            vec![],
            vec![],
            None,
            io.clone(),
            vec![],
            token.clone(),
        );

        let mut exit_rx = session
            .child_reaper
            .watch_grandchild(child, vec![])
            .context("watch grandchild for pid")?;

        let (stdout_rx, stderr_rx) = io
            .stdio()
            .await
            .context("retrieve stdout and stderr channels")?;

        let attach = io.attach().await;

        loop {
            tokio::select! {
                Some(data) = stdin_rx.recv()  => if session.stdin {
                    // First element is the message type indicator
                    if let Some((&msg_type, payload)) = data.split_first() {
                        match msg_type {
                            STDIN_BYTE => {
                                trace!("Got stdin message of len {}", payload.len());
                                attach.stdin().send(Arc::from(payload)).context("send to attach session")?;
                            },
                            RESIZE_BYTE => {
                                let e = serde_json::from_slice::<ResizeEvent>(payload).context("unmarshal resize event")?;
                                trace!("Got resize message: {e:?}");
                                io.resize(e.width, e.height).await.context("resize terminal")?;
                            },
                            CLOSE_BYTE => {
                                info!("Got close message");
                                break
                            },
                            x => warn!("Unknown start byte for stdin: {x}"),
                        }
                    }
                },

                Ok(IOMessage::Data(data, _)) = stdout_rx.recv() => if session.stdout {
                    Self::frame_and_send(STDOUT_BYTE, &data, sender).await
                        .context("send to stdout")?;
                },

                Ok(IOMessage::Data(data, _)) = stderr_rx.recv() => if session.stderr {
                    Self::frame_and_send(STDERR_BYTE, &data, sender).await
                        .context("send to stderr")?;
                },

                Ok(exit_data) = exit_rx.recv() => {
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
        mut stdin_rx: MpscReceiver<Vec<u8>>,
    ) -> Result<()> {
        let io = session.child.io();

        let (stdout_rx, stderr_rx) = io
            .stdio()
            .await
            .context("retrieve stdout and stderr channels")?;

        let attach = io.attach().await;

        loop {
            tokio::select! {
                Some(data) = stdin_rx.recv()  => if session.stdin {
                    // First element is the message type indicator
                    if let Some((&msg_type, payload)) = data.split_first() {
                        match msg_type {
                            STDIN_BYTE => {
                                trace!("Got stdin message of len {}", payload.len());
                                attach.stdin().send(Arc::from(payload)).context("send to attach session")?;
                            },
                            RESIZE_BYTE => {
                                let e = serde_json::from_slice::<ResizeEvent>(payload).context("unmarshal resize event")?;
                                trace!("Got resize message: {e:?}");
                                io.resize(e.width, e.height).await.context("resize terminal")?;
                            },
                            CLOSE_BYTE => {
                                info!("Got close message");
                                break
                            },
                            x => warn!("Unknown start byte for stdin: {x}"),
                        }
                    }
                },

                Ok(IOMessage::Data(data, _)) = stdout_rx.recv() => if session.stdout {
                    Self::frame_and_send(STDOUT_BYTE, &data, sender).await
                        .context("send to stdout")?;
                },

                Ok(IOMessage::Data(data, _)) = stderr_rx.recv() => if session.stderr {
                    Self::frame_and_send(STDERR_BYTE, &data, sender).await
                        .context("send to stderr")?;
                },

                _ = session.child.token().cancelled() => {
                    debug!("Exiting streaming attach because token cancelled");
                    break
                }
            }
        }

        Ok(())
    }

    /// The port forward specific read/write loop.
    async fn port_forward_loop(
        _session: PortForwardSession,
        _sender: &mut SplitSink<WebSocket, Message>,
        mut _in_rx: MpscReceiver<Vec<u8>>,
    ) -> Result<()> {
        todo!("Requires SPDY protocol implementation from https://github.com/moby/spdystream")
    }

    /// Prepend a stream type byte and send the data over WebSocket.
    async fn frame_and_send(
        stream_byte: u8,
        data: &[u8],
        sender: &mut SplitSink<WebSocket, Message>,
    ) -> Result<()> {
        let mut framed = Vec::with_capacity(1 + data.len());
        framed.push(stream_byte);
        framed.extend_from_slice(data);
        sender
            .send(Message::Binary(framed.into()))
            .await
            .context("send framed data")
    }
}
