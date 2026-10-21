use crate::conmon_capnp::conmon::CgroupManager;
use crate::{
    attach::SharedContainerAttach,
    bounded_hashmap::BoundedHashMap,
    child::{Child, ChildKind},
    child_reaper::{ChildReaper, ExitChannelData, ReapableChild},
    config::Config,
    container_io::{ContainerIO, Drain, EarlyStdio, Message as IOMessage, Pipe, SharedContainerIO},
    port_forward::{self, PortForward, WebSocketProtocol},
    process::Process,
    server::GenerateRuntimeArgs,
};
use anyhow::{Context, Result};
use async_channel::Receiver as IOReceiver;
use axum::{
    Router,
    body::Body,
    extract::{
        FromRequestParts, Path, Request, State as AxumState,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code},
    },
    http::{HeaderMap, HeaderName, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::{BufMut, Bytes, BytesMut};
use futures::{
    sink::SinkExt,
    stream::{SplitSink, SplitStream, StreamExt},
};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use libc::pid_t;
use nix::{sys::signal::Signal, unistd::Pid};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fmt::Debug,
    future,
    ops::ControlFlow,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    net::TcpListener,
    select,
    sync::{
        RwLock,
        broadcast::{self, error::RecvError},
        mpsc::{self, Receiver as MpscReceiver, Sender as MpscSender},
        watch,
    },
    task::{self, JoinHandle},
    time::{self, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, info, trace, warn};
use uuid::Uuid;

const ADDR: &str = "127.0.0.1";

const PROTOCOL_V5: &str = "v5.channel.k8s.io";

const EXEC_PATH: &str = "exec";
const ATTACH_PATH: &str = "attach";
const PORT_FORWARD_PATH: &str = "portforward";

/// The upgrade port forward clients ask for.
const SPDY_31: &str = "SPDY/3.1";

/// The streaming protocols of a SPDY upgrade request, and the one the server
/// picked in the response.
const PROTOCOL_VERSION_HEADER: HeaderName = HeaderName::from_static("x-stream-protocol-version");

/// The protocols the server supports, if it supports none of the client.
const ACCEPTED_PROTOCOL_VERSIONS_HEADER: HeaderName =
    HeaderName::from_static("x-accepted-stream-protocol-versions");

const STDIN_BYTE: u8 = 0;
const STDOUT_BYTE: u8 = 1;
const STDERR_BYTE: u8 = 2;
const STREAM_ERR_BYTE: u8 = 3;
const RESIZE_BYTE: u8 = 4;
const CLOSE_BYTE: u8 = 255;

/// The largest WebSocket message and frame a client may send. Clients send
/// stdin, which kubectl reads in 32 KiB chunks and the attach sockets cap at
/// 64 KiB per packet, and small resize events. The defaults of 64 MiB per
/// message would let a client make the server buffer that much per message.
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// How much stdin the read task holds while the container does not take it,
/// on top of the stdin channel. The socket is still read meanwhile, so that
/// resize events and the close do not wait behind it.
const PENDING_STDIN_MAX_BYTES: usize = MAX_MESSAGE_SIZE;

/// The largest frame of the output collected while the runtime of an exec
/// ran, like the largest reads of the pipes.
const EARLY_FRAME_SIZE: usize = 32 * 1024;

/// How often a session pings the client while its stdin waits for the
/// container, to notice a client which went away.
const STDIN_PING_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(5)
};

/// How long stdin which the client sent right before closing may take to
/// reach the container.
const STDIN_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// The latest resize event of a client, older ones are replaced.
type ResizeRx = watch::Receiver<Option<Bytes>>;

#[derive(Debug, Default)]
/// The main streaming server structure of this module.
pub struct StreamingServer {
    running: bool,
    port: u16,
    state: Arc<RwLock<State>>,
}

/// State handled by the streaming server, the sessions waiting for their
/// client by token.
#[derive(Debug, Default)]
struct State {
    sessions: BoundedHashMap<Uuid, Session>,
    port_forwards: BoundedHashMap<Uuid, PortForward>,
}

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
    container_id: Box<str>,
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

/// The status of a session which succeeded on the error stream. The kubelet
/// sends it as well, some clients take an empty error stream for a failure.
const SUCCESS_FRAME: &[u8] = b"\x03{\"metadata\":{},\"status\":\"Success\"}";

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
            // Clients upgrade port forward requests to SPDY with a POST.
            .route(
                &Self::path_for(PORT_FORWARD_PATH),
                get(Self::handle_port_forward).post(Self::handle_port_forward),
            )
            .fallback(Self::fallback)
            .with_state(state)
            .layer(middleware::from_fn(Self::trace));
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

    /// Log every request with its response status.
    async fn trace(request: Request, next: Next) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let response = next.run(request).await;
        debug!("{method} {path}: {}", response.status());
        response
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
        state_lock.sessions.insert(
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
        container_id: Box<str>,
        child: ReapableChild,
        stdin: bool,
        stdout: bool,
        stderr: bool,
        stop_after_stdin_eof: bool,
    ) -> String {
        let mut state_lock = self.state.write().await;
        let uuid = Uuid::new_v4();
        state_lock.sessions.insert(
            uuid,
            Session::Attach(AttachSession {
                container_id,
                child,
                stdin,
                stdout,
                stderr,
                stop_after_stdin_eof,
            }),
        );
        self.url_for(ATTACH_PATH, &uuid)
    }

    /// Returns the URL used for forwarding ports into the network namespace.
    pub async fn port_forward_url(
        &self,
        netns_path: PathBuf,
        idle_timeout: Option<Duration>,
    ) -> String {
        let uuid = Uuid::new_v4();
        self.state
            .write()
            .await
            .port_forwards
            .insert(uuid, PortForward::new(netns_path, idle_timeout));
        self.url_for(PORT_FORWARD_PATH, &uuid)
    }

    /// Handle a webserver connection which should be upgraded to become a websocket one.
    async fn handle(
        ws: WebSocketUpgrade,
        Path(token): Path<Uuid>,
        AxumState(state): AxumState<Arc<RwLock<State>>>,
    ) -> impl IntoResponse {
        async move {
            info!("Got request for token: {token}");
            let session = state.write().await.sessions.remove(&token);

            match session {
                Some(session) => {
                    // Only what identifies the session, the command of an exec
                    // may carry secrets.
                    info!(
                        "Got valid {} session for container {} and token {token}",
                        session.typ(),
                        session.container_id()
                    );
                    ws.protocols([PROTOCOL_V5])
                        .max_message_size(MAX_MESSAGE_SIZE)
                        .max_frame_size(MAX_MESSAGE_SIZE)
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

    /// Handle a port forward request, which the client upgrades to SPDY.
    async fn handle_port_forward(
        Path(token): Path<Uuid>,
        AxumState(state): AxumState<Arc<RwLock<State>>>,
        mut request: Request,
    ) -> Response {
        async move {
            info!("Got port forward request for token: {token}");
            let Some(session) = state.write().await.port_forwards.remove(&token) else {
                error!("Unable to find port forward session for token: {token}");
                return StatusCode::NOT_FOUND.into_response();
            };
            if Self::is_websocket_request(request.headers()) {
                return Self::upgrade_port_forward_websocket(token, session, request).await;
            }
            if let Some(response) = Self::reject_spdy_handshake(request.headers()) {
                return response;
            }
            let Some(on_upgrade) = request.extensions_mut().remove::<OnUpgrade>() else {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "unable to upgrade: connection does not support upgrades",
                )
                    .into_response();
            };

            task::spawn(
                async move {
                    match on_upgrade.await {
                        Ok(upgraded) => session.serve_spdy(TokioIo::new(upgraded)).await,
                        Err(e) => error!("Unable to upgrade port forward connection: {e}"),
                    }
                }
                .instrument(debug_span!("port_forward", %token)),
            );
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header(header::CONNECTION, "Upgrade")
                .header(header::UPGRADE, SPDY_31)
                .header(PROTOCOL_VERSION_HEADER, port_forward::PROTOCOL)
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        .instrument(debug_span!("handle_port_forward", %token))
        .await
    }

    fn is_websocket_request(headers: &HeaderMap) -> bool {
        headers
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
    }

    /// Upgrade a port forward request to a WebSocket, which tunnels SPDY or
    /// speaks the channel protocol, by the subprotocols of the client.
    async fn upgrade_port_forward_websocket(
        token: Uuid,
        session: PortForward,
        request: Request,
    ) -> Response {
        let requested: Vec<_> = request
            .headers()
            .get_all(header::SEC_WEBSOCKET_PROTOCOL)
            .into_iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        let Some((protocol, subprotocol)) =
            WebSocketProtocol::negotiate(requested.iter().map(String::as_str))
        else {
            return (
                StatusCode::FORBIDDEN,
                format!(
                    "unable to upgrade: requested protocol(s) are not supported: {requested:?}"
                ),
            )
                .into_response();
        };
        let ports = match protocol {
            WebSocketProtocol::SpdyTunnel => vec![],
            WebSocketProtocol::Channels { .. } => {
                match PortForward::query_ports(request.uri().query()) {
                    Ok(ports) => ports,
                    Err(e) => return (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
                }
            }
        };

        let (mut parts, _) = request.into_parts();
        let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(ws) => ws,
            Err(rejection) => return rejection.into_response(),
        };
        let ws = match subprotocol {
            Some(subprotocol) => ws.protocols([subprotocol]),
            None => ws,
        };
        ws.max_message_size(MAX_MESSAGE_SIZE)
            .max_frame_size(MAX_MESSAGE_SIZE)
            .on_upgrade(move |socket| {
                session
                    .serve_websocket(socket, protocol, ports)
                    .instrument(debug_span!("port_forward", %token))
            })
    }

    /// The response which rejects the request, unless it asks for a SPDY/3.1
    /// upgrade and the port forward protocol. The responses are the ones of
    /// the SPDY server of Kubernetes.
    fn reject_spdy_handshake(headers: &HeaderMap) -> Option<Response> {
        let values = |name| {
            headers
                .get_all(name)
                .into_iter()
                .filter_map(|v| v.to_str().ok())
        };
        let contains = |name, value: &str| {
            values(name).any(|v| v.to_ascii_lowercase().contains(&value.to_ascii_lowercase()))
        };
        if !contains(header::CONNECTION, "upgrade") || !contains(header::UPGRADE, SPDY_31) {
            return Some(
                (
                    StatusCode::BAD_REQUEST,
                    "unable to upgrade: port forwarding needs a SPDY/3.1 upgrade",
                )
                    .into_response(),
            );
        }

        let protocols: Vec<_> = values(PROTOCOL_VERSION_HEADER)
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if protocols.is_empty() {
            return Some(
                (
                    StatusCode::BAD_REQUEST,
                    format!("unable to upgrade: header {PROTOCOL_VERSION_HEADER} is required"),
                )
                    .into_response(),
            );
        }
        if !protocols.contains(&port_forward::PROTOCOL) {
            return Some((
                StatusCode::FORBIDDEN,
                [(ACCEPTED_PROTOCOL_VERSIONS_HEADER, port_forward::PROTOCOL)],
                format!(
                    "unable to upgrade: unable to negotiate protocol: client supports {protocols:?}, server accepts [{}]",
                    port_forward::PROTOCOL
                ),
            )
                .into_response());
        }
        None
    }

    /// Handle a single websocket connection.
    async fn handle_websocket(socket: WebSocket, session: Session) {
        let (sender, receiver) = socket.split();
        let (stdin_tx, stdin_rx) = mpsc::channel(16);
        // Resize events go through their own channel, so that they do not
        // queue behind stdin the container does not read. Only the latest
        // size matters.
        let (resize_tx, resize_rx) = watch::channel(None);

        let peer_closed = Arc::new(AtomicBool::new(false));
        let sender = Output::new(sender, peer_closed.clone());
        let mut send_task = Self::write_task(sender, stdin_rx, resize_rx, session);
        let mut recv_task = Self::read_task(receiver, stdin_tx, resize_tx, peer_closed);

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
                    // After a clean close the session forwards the stdin which
                    // the client sent before, for a short time.
                    Ok(true) => {
                        info!("Client closed the connection");
                        if time::timeout(STDIN_FLUSH_TIMEOUT, &mut send_task).await.is_err() {
                            debug!("Session did not end after the close, stopping it");
                        }
                    }
                    Ok(false) => info!("Client connection ended"),
                    Err(b) => error!("Error receiving messages: {b:?}")
                }
                send_task.abort();
            }
        }

        info!("Closing websocket connection");
    }

    /// Build a common write task based on the session type.
    fn write_task(
        sender: Output,
        stdin_rx: MpscReceiver<Bytes>,
        resize_rx: ResizeRx,
        session: Session,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut sender = sender;
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
                debug!("Unable to send close message: {e:#}")
            }
        })
    }

    /// Build a common read task.
    fn read_task(
        mut receiver: SplitStream<WebSocket>,
        stdin_tx: MpscSender<Bytes>,
        resize_tx: watch::Sender<Option<Bytes>>,
        peer_closed: Arc<AtomicBool>,
    ) -> JoinHandle<bool> {
        tokio::spawn(async move {
            let mut pending = PendingStdin::default();
            let mut closed = false;
            loop {
                select! {
                    permit = stdin_tx.reserve(), if !pending.is_empty() => match permit {
                        Ok(permit) => permit.send(pending.pop()),
                        // The session ended, nothing takes stdin anymore.
                        Err(_) => pending.clear(),
                    },
                    msg = receiver.next(), if !pending.is_full() => match msg {
                        Some(Ok(msg)) => {
                            if Self::read_message(msg, &mut pending, &resize_tx).is_break() {
                                peer_closed.store(true, Ordering::Release);
                                closed = true;
                                break;
                            }
                        }
                        _ => break,
                    },
                }
            }

            // Stdin sent right before a clean close, like the close of the
            // stream itself, still has to reach the container. Only for a
            // short time, a container which does not read it must not delay
            // the end of the session. A client which went away gets nothing
            // forwarded anymore.
            if closed {
                let flush = async {
                    while !pending.is_empty() {
                        if stdin_tx.send(pending.pop()).await.is_err() {
                            break;
                        }
                    }
                };
                if time::timeout(STDIN_FLUSH_TIMEOUT, flush).await.is_err() {
                    debug!("Dropping stdin the container did not take after the close");
                }
            }
            closed
        })
    }

    /// Read a single message and return the control flow decision.
    fn read_message(
        msg: Message,
        pending: &mut PendingStdin,
        resize_tx: &watch::Sender<Option<Bytes>>,
    ) -> ControlFlow<(), ()> {
        match msg {
            Message::Binary(data) if !data.is_empty() => {
                debug!("Got {} binary bytes", data.len());
                if data[0] == RESIZE_BYTE {
                    resize_tx.send_replace(Some(data));
                } else {
                    pending.push(data);
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

    /// Run the runtime of an exec session and register the process it
    /// started.
    async fn start_exec(mut session: ExecSession) -> Result<StartedExec> {
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

        // Read while the runtime runs, see `ContainerIO::collect_stdio`.
        session
            .container_io
            .collect_stdio(session.stdout, session.stderr)
            .context("collect exec output")?;

        let res = session
            .child_reaper
            .create_child(
                session.server_config.runtime(),
                &args,
                session.stdin,
                &mut session.container_io,
                pidfile.path(),
                None,
                vec![],
                false,
                vec![],
            )
            .await;
        let (grandchild_pid, token, call) = match res {
            Ok(res) => res,
            Err(e) => {
                let e = session.container_io.runtime_error(e).await;
                return Err(e.context("create new child process"));
            }
        };
        // The runtime wrote the PID, the file served its purpose.
        drop(pidfile);
        // Not reaped before the registration below, so the PID is still its
        // own.
        let process = Process::open(Pid::from_raw(grandchild_pid as pid_t));
        let early_stdio = session.container_io.take_early_stdio().await;

        let io = SharedContainerIO::new(session.container_io);
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
        let exit_rx = session
            .child_reaper
            .watch_grandchild(child, vec![], call)
            .context("watch grandchild for pid")?;

        let mut teardown = ExecTeardown {
            stdio: None,
            attach: io.attach().await,
            token,
            process,
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

    /// The exec specific read/write loop.
    async fn exec_loop(
        session: ExecSession,
        sender: &mut Output,
        mut stdin_rx: MpscReceiver<Bytes>,
        mut resize_rx: ResizeRx,
    ) -> Result<()> {
        // The runtime runs in a task of its own. A client which goes away
        // meanwhile must not kill it halfway, the process it started still
        // gets registered, and torn down once the task finished.
        let StartedExec {
            io,
            early_stdio,
            mut exit_rx,
            stdin_enabled,
            stdout_enabled,
            stderr_enabled,
            teardown,
        } = task::spawn(Self::start_exec(session))
            .await
            .context("join exec start")??;
        let (stdout_rx, stderr_rx) = teardown.stdio.clone().context("no stdio channels")?;
        let attach = teardown.attach.clone();
        let mut pending_stdin = None;
        let mut stdin_ping = Self::stdin_ping();
        let mut stdin_closed = false;

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
                            Self::frame_and_send(stream_byte, &frame, sender).await?;
                        }
                    }
                },

                res = stdout_rx.recv(), if !stdout_done && early_stdout.is_empty() => match res {
                    Ok(IOMessage::Data(data, _)) => {
                        Self::frame_and_send(STDOUT_BYTE, &data, sender).await?;
                    }
                    Ok(IOMessage::Done) | Err(_) => stdout_done = true,
                },

                res = stderr_rx.recv(), if !stderr_done && early_stderr.is_empty() => match res {
                    Ok(IOMessage::Data(data, _)) => {
                        Self::frame_and_send(STDERR_BYTE, &data, sender).await?;
                    }
                    Ok(IOMessage::Done) | Err(_) => stderr_done = true,
                },

                // Resizing does not depend on stdin, a terminal without it
                // still has a size.
                Ok(()) = resize_rx.changed() => {
                    let data = resize_rx.borrow_and_update().clone();
                    if let Some(data) = data
                        && let Err(e) = Self::handle_resize(&data, &io).await
                    {
                        warn!("Ignoring resize event: {e:#}");
                    }
                },

                // Wait for the container to take the stdin without blocking the
                // output meanwhile.
                () = attach.send_stdin(pending_stdin.clone().unwrap_or_default()), if pending_stdin.is_some() => {
                    pending_stdin = None
                },

                // The stdin of the client is not read while it waits for the
                // container, so a client which goes away meanwhile is only
                // noticed by a failing send.
                _ = stdin_ping.tick(), if pending_stdin.is_some() => {
                    sender.send(Message::Ping(Bytes::new())).await?
                },

                res = stdin_rx.recv(), if pending_stdin.is_none() => match res {
                    // The client closed the connection, everything it sent
                    // reached the process.
                    None => break,
                    Some(_) if !stdin_enabled || stdin_closed => {}
                    Some(data) => match Self::handle_stdin(data) {
                        // The client closed its stdin. Close the process stdin
                        // as well and keep forwarding the output until it
                        // exits. A terminal cannot be closed without ending
                        // its output, it gets its end of file character
                        // instead, like from a user pressing Ctrl-D. A process
                        // reading lines sees the end of its input then, one
                        // after a line without its ending needs a second one,
                        // like a user would type it.
                        ControlFlow::Break(()) => {
                            stdin_closed = true;
                            match io.terminal_eof().await {
                                Some(eof) => pending_stdin = Some(Bytes::from(vec![eof])),
                                None => {
                                    attach.close_container_stdin();
                                }
                            }
                        }
                        ControlFlow::Continue(stdin) => pending_stdin = stdin,
                    },
                },

                Ok(exit_data) = exit_rx.recv() => {
                    // Forward what the read loops still deliver before closing.
                    for (frames, stream_byte) in [(&mut early_stdout, STDOUT_BYTE), (&mut early_stderr, STDERR_BYTE)] {
                        for frame in frames.drain(..) {
                            Self::frame_and_send(stream_byte, &frame, sender).await?;
                        }
                    }
                    if !stdout_done {
                        Self::drain_stdio(&stdout_rx, STDOUT_BYTE, sender).await?;
                    }
                    if !stderr_done {
                        Self::drain_stdio(&stderr_rx, STDERR_BYTE, sender).await?;
                    }
                    if exit_data.exit_code == 0 {
                        Self::send_success(sender).await?;
                    } else {
                        Self::send_status(sender, &ErrorMessage::new(exit_data.exit_code)).await?;
                    }
                    break
                },
            }
        }

        Ok(())
    }

    /// The interval of the pings while the stdin of a session waits for the
    /// container.
    fn stdin_ping() -> time::Interval {
        let mut ping = time::interval_at(
            time::Instant::now() + STDIN_PING_INTERVAL,
            STDIN_PING_INTERVAL,
        );
        ping.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        ping
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

    /// The attach specific read/write loop.
    async fn attach_loop(
        session: AttachSession,
        sender: &mut Output,
        mut stdin_rx: MpscReceiver<Bytes>,
        mut resize_rx: ResizeRx,
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
        let terminal = attach.is_terminal();
        let mut pending_stdin = None;
        let mut stdin_ping = Self::stdin_ping();
        // Without stdin, there is none to wait for.
        let mut stdin_closed = !stdin_enabled;

        // With stdin once, the end of the session closes the container stdin,
        // also if the client went away without closing it, like the first
        // session ending does with StdinOnce in Kubernetes.
        let mut teardown = AttachTeardown {
            attach: attach.clone(),
            token: token.clone(),
            close_stdin: stdin_enabled && stop_after_stdin_eof,
        };

        // A single `Done` follows once all pipes ended. It may have been sent
        // before subscribing, then only forward what is still queued.
        let (mut attach_rx, mut done) = attach.subscribe();
        if done {
            while let Ok(message) = attach_rx.try_recv() {
                Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
            }
        }

        // Until nothing is left to forward in either direction.
        while !(done && stdin_closed) {
            tokio::select! {
                res = attach_rx.recv(), if !done => match res {
                    Ok(IOMessage::Done) => done = true,
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
                Ok(()) = resize_rx.changed() => {
                    let data = resize_rx.borrow_and_update().clone();
                    if let Some(data) = data
                        && let Err(e) = Self::handle_resize(&data, &io).await
                    {
                        warn!("Ignoring resize event: {e:#}");
                    }
                },

                // Wait for the container to take the stdin without blocking the
                // output meanwhile.
                () = attach.send_stdin(pending_stdin.clone().unwrap_or_default()), if pending_stdin.is_some() => {
                    pending_stdin = None
                },

                // The stdin of the client is not read while it waits for the
                // container, so a client which goes away meanwhile is only
                // noticed by a failing send.
                _ = stdin_ping.tick(), if pending_stdin.is_some() => {
                    sender.send(Message::Ping(Bytes::new())).await?
                },

                res = stdin_rx.recv(), if pending_stdin.is_none() => match res {
                    // The client closed the connection, everything it sent
                    // reached the container. Nobody gets a status anymore.
                    None => return Ok(()),
                    Some(_) if stdin_closed => {}
                    Some(data) => match Self::handle_stdin(data) {
                        // The client closed its stdin. Without a terminal and
                        // stdin once, that ends the session, like for CRI-O
                        // with conmon. Otherwise keep forwarding the output
                        // until it ends, and with stdin once close the
                        // container stdin.
                        ControlFlow::Break(()) if !terminal && !stop_after_stdin_eof => break,
                        ControlFlow::Break(()) => {
                            stdin_closed = true;
                            if stop_after_stdin_eof {
                                teardown.close_stdin = false;
                                // Like for exec, a terminal gets its end of
                                // file character behind the earlier stdin.
                                match io.terminal_eof().await {
                                    Some(eof) => pending_stdin = Some(Bytes::from(vec![eof])),
                                    None => {
                                        attach.close_container_stdin();
                                    }
                                }
                            }
                        }
                        ControlFlow::Continue(stdin) => pending_stdin = stdin,
                    },
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

        Self::send_success(sender).await?;
        Ok(())
    }

    /// Forward a single output message to the WebSocket client.
    async fn forward_output(
        message: IOMessage,
        stdout_enabled: bool,
        stderr_enabled: bool,
        sender: &mut Output,
    ) -> Result<()> {
        match message {
            IOMessage::Data(data, Pipe::StdOut) if stdout_enabled => {
                Self::frame_and_send(STDOUT_BYTE, &data, sender).await
            }
            IOMessage::Data(data, Pipe::StdErr) if stderr_enabled => {
                Self::frame_and_send(STDERR_BYTE, &data, sender).await
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
        sender: &mut Output,
    ) -> Result<()> {
        let mut drain = Drain::default();
        while let Some(message) = drain.recv_output(attach_rx, true).await {
            Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
        }
        Ok(())
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
                    Self::frame_and_send(stream_byte, &data, sender).await?;
                }
                Some(Ok(IOMessage::Done)) | Some(Err(_)) | None => return Ok(()),
            }
        }
    }

    /// Send a session error on the error stream.
    async fn send_error(sender: &mut Output, message: String) -> Result<()> {
        Self::send_status(sender, &ErrorMessage::internal(message)).await
    }

    /// Send the success status of a session on the error stream.
    async fn send_success(sender: &mut Output) -> Result<()> {
        sender
            .send(Message::Binary(Bytes::from_static(SUCCESS_FRAME)))
            .await
    }

    /// Send the final status of a session on the error stream.
    async fn send_status(sender: &mut Output, status: &impl Serialize) -> Result<()> {
        sender
            .send(Message::Binary(Self::status_frame(status)?.into()))
            .await
    }

    fn status_frame(status: &impl Serialize) -> Result<Vec<u8>> {
        let mut frame = vec![STREAM_ERR_BYTE];
        serde_json::to_writer(&mut frame, status).context("serialize status message")?;
        Ok(frame)
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
    async fn frame_and_send(stream_byte: u8, data: &[u8], sender: &mut Output) -> Result<()> {
        let mut frame = BytesMut::with_capacity(data.len() + 1);
        frame.put_u8(stream_byte);
        frame.extend_from_slice(data);
        sender.send(Message::Binary(frame.freeze())).await
    }
}

/// The sending half of a session. A failing send means that the client went
/// away, which ends the session. After a clean close of the client, sends
/// fail as well, the output gets dropped then. The session still forwards
/// the stdin which the client sent before its close to the process, see
/// [`STDIN_FLUSH_TIMEOUT`], and reads the output meanwhile, so that the
/// process does not wait for it.
struct Output {
    sender: SplitSink<WebSocket, Message>,
    failed: bool,

    /// Set once the client closed the connection cleanly.
    peer_closed: Arc<AtomicBool>,
}

impl Output {
    fn new(sender: SplitSink<WebSocket, Message>, peer_closed: Arc<AtomicBool>) -> Self {
        Self {
            sender,
            failed: false,
            peer_closed,
        }
    }

    /// Send a message. A failure ends the session, unless the client closed
    /// the connection cleanly, see [`Output`].
    async fn send(&mut self, message: Message) -> Result<()> {
        if self.failed {
            return Ok(());
        }
        match self.sender.send(message).await {
            Ok(()) => Ok(()),
            Err(e) if self.peer_closed.load(Ordering::Acquire) => {
                debug!("Dropping the output of the session after the close: {e}");
                self.failed = true;
                Ok(())
            }
            Err(e) => Err(e).context("send to client"),
        }
    }
}

impl Session {
    /// The kind of the session for the logs.
    fn typ(&self) -> &'static str {
        match self {
            Session::Exec(_) => "exec",
            Session::Attach(_) => "attach",
        }
    }

    fn container_id(&self) -> &str {
        match self {
            Session::Exec(s) => &s.container_id,
            Session::Attach(s) => &s.container_id,
        }
    }
}

/// Stdin of a client which the container did not take yet, bounded by
/// [`PENDING_STDIN_MAX_BYTES`].
#[derive(Debug, Default)]
struct PendingStdin {
    queue: VecDeque<Bytes>,
    bytes: usize,
}

impl PendingStdin {
    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Whether the client has to wait, which stops reading the socket.
    fn is_full(&self) -> bool {
        self.bytes >= PENDING_STDIN_MAX_BYTES
    }

    fn push(&mut self, data: Bytes) {
        self.bytes += data.len();
        self.queue.push_back(data);
    }

    /// The oldest message, callers check that there is one.
    fn pop(&mut self) -> Bytes {
        let data = self.queue.pop_front().unwrap_or_default();
        self.bytes -= data.len();
        data
    }

    fn clear(&mut self) {
        self.queue.clear();
        self.bytes = 0;
    }
}

/// An exec process which got started and registered for a session.
struct StartedExec {
    io: SharedContainerIO,

    /// The output written while the runtime ran.
    early_stdio: EarlyStdio,
    exit_rx: broadcast::Receiver<ExitChannelData>,
    stdin_enabled: bool,
    stdout_enabled: bool,
    stderr_enabled: bool,
    teardown: ExecTeardown,
}

/// Ends an exec session however it ends, also if the client went away and
/// the loop got aborted, or before the process even got started. The loop
/// only ends before the exit of the process if the WebSocket is gone.
struct ExecTeardown {
    /// The stdout and stderr channels of the process.
    stdio: Option<(IOReceiver<IOMessage>, IOReceiver<IOMessage>)>,
    attach: SharedContainerAttach,

    /// Cancelled once the process exited.
    token: CancellationToken,

    /// The process, which its watcher may reap before the token got
    /// cancelled, so it does not get signalled by its PID.
    process: Process,
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
        // Without a client the process would wait for stdin forever. Close
        // it, like the end of the session would for a process started by the
        // client itself. A terminal cannot be closed without ending its
        // output, hang it up instead, like a closed connection does.
        if !self.attach.close_container_stdin() {
            self.hang_up();
        }
    }
}

/// Closes the container stdin when an attach session with stdin once ends
/// while the container runs, also if the client went away without closing
/// its stdin and the loop got aborted.
struct AttachTeardown {
    attach: SharedContainerAttach,

    /// Cancelled once the container exited.
    token: CancellationToken,

    /// Whether the container stdin still has to be closed.
    close_stdin: bool,
}

impl Drop for AttachTeardown {
    fn drop(&mut self) {
        if self.close_stdin && !self.token.is_cancelled() {
            debug!("Closing the container stdin after the end of the attach session");
            self.attach.close_container_stdin();
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
        container_log::ContainerLog,
        spdy::testing::{Client, Frame, Headers},
    };
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        task::LocalSet,
    };

    /// A masked client frame, as a WebSocket client sends it.
    fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [1, 2, 3, 4];
        let mut frame = vec![0x80 | opcode];
        match u16::try_from(payload.len()) {
            Ok(len) if len < 126 => frame.push(0x80 | len as u8),
            Ok(len) => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&len.to_be_bytes());
            }
            Err(_) => panic!("payload too large"),
        }
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        frame
    }

    /// A container without a terminal and an attach session for it.
    async fn attach_session(
        stdin: bool,
        stop_after_stdin_eof: bool,
    ) -> Result<(SharedContainerIO, StreamingServer, TcpStream)> {
        attach_session_with(false, stdin, stop_after_stdin_eof).await
    }

    /// A container and an attach session for it.
    async fn attach_session_with(
        terminal: bool,
        stdin: bool,
        stop_after_stdin_eof: bool,
    ) -> Result<(SharedContainerIO, StreamingServer, TcpStream)> {
        let io = SharedContainerIO::new(ContainerIO::new(terminal, ContainerLog::new())?);
        let (_, child) = ReapableChild::from_child(Child {
            id: "id".into(),
            kind: ChildKind::Container,
            pid: 0,
            exit_paths: vec![],
            oom_exit_paths: vec![],
            timeout: None,
            io: io.clone(),
            cleanup_cmd: vec![],
            token: CancellationToken::new(),
        });
        let mut server = StreamingServer::default();
        server.start_if_required().await?;
        let url = server
            .attach_url("id".into(), child, stdin, true, false, stop_after_stdin_eof)
            .await;
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
        let mut response = vec![];
        while !response.ends_with(b"\r\n\r\n") {
            response.push(stream.read_u8().await?);
        }
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 101"));
        Ok((io, server, stream))
    }

    /// Read a frame the server sent, its opcode and payload.
    async fn server_frame<R>(stream: &mut R) -> Result<(u8, Vec<u8>)>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let mut header = [0; 2];
        time::timeout(Duration::from_secs(5), stream.read_exact(&mut header)).await??;
        let len = match header[1] & 0x7f {
            126 => usize::from(stream.read_u16().await?),
            127 => usize::try_from(stream.read_u64().await?)?,
            len => usize::from(len),
        };
        let mut payload = vec![0; len];
        stream.read_exact(&mut payload).await?;
        Ok((header[0] & 0x0f, payload))
    }

    #[tokio::test]
    async fn stdin_before_close_reaches_the_container() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut stream) = attach_session(true, false).await?;

                // More stdin than the channels on the way hold, right before
                // the close, in one write.
                let mut frames = vec![];
                for i in 0..50u8 {
                    frames.extend(client_frame(0x2, &[STDIN_BYTE, i]));
                }
                frames.extend(client_frame(0x8, &1000u16.to_be_bytes()));
                stream.write_all(&frames).await?;

                // The container writes output meanwhile, which cannot be sent
                // after the close anymore.
                let mut attach = io.attach().await;
                for i in 0..50u8 {
                    attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                    let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                    assert_eq!(data, [i].as_slice());
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn attach_ends_with_success_on_stdin_close() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut stream) = attach_session(true, false).await?;
                stream.write_all(&client_frame(0x2, &[CLOSE_BYTE])).await?;

                assert_eq!(
                    server_frame(&mut stream).await?,
                    (
                        0x2,
                        [
                            &[STREAM_ERR_BYTE][..],
                            br#"{"metadata":{},"status":"Success"}"#
                        ]
                        .concat()
                    )
                );
                assert_eq!(server_frame(&mut stream).await?.0, 0x8);
                // Without stdin once, the container stdin stays open.
                assert!(!io.attach().await.stdin_closed().is_cancelled());
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn attach_closes_stdin_once_on_disconnect() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, stream) = attach_session(true, true).await?;
                let attach = io.attach().await;
                drop(stream);
                time::timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn attach_ends_on_disconnect_while_stdin_waits() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut stream) = attach_session(true, true).await?;
                let attach = io.attach().await;

                // More stdin than the session holds while nothing reads it,
                // so the socket is not read anymore.
                let mut frame = vec![STDIN_BYTE];
                frame.resize(60_000, b'x');
                let frame = client_frame(0x2, &frame);
                for _ in 0..PENDING_STDIN_MAX_BYTES / 60_000 + 40 {
                    stream.write_all(&frame).await?;
                }
                time::sleep(Duration::from_millis(200)).await;
                assert!(!attach.stdin_closed().is_cancelled());
                drop(stream);

                // The session notices and ends, which closes the stdin once.
                time::timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn terminal_attach_gets_eof_on_stdin_close() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut stream) = attach_session_with(true, true, true).await?;
                let mut frames = client_frame(0x2, &[STDIN_BYTE, b'a']);
                frames.extend(client_frame(0x2, &[CLOSE_BYTE]));
                stream.write_all(&frames).await?;

                // The end of file character follows the stdin, like a user
                // pressing Ctrl-D.
                let mut attach = io.attach().await;
                for expected in [&b"a"[..], &[0x04]] {
                    let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                    assert_eq!(data, expected);
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn attach_without_stdin_ignores_stdin_close() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut stream) = attach_session(false, true).await?;
                let attach = io.attach().await;
                stream.write_all(&client_frame(0x2, &[CLOSE_BYTE])).await?;

                // The output still reaches the client.
                time::timeout(Duration::from_secs(5), async {
                    while !attach.has_readers() {
                        time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await?;
                attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                assert_eq!(
                    server_frame(&mut stream).await?,
                    (0x2, vec![STDOUT_BYTE, b'o', b'u', b't'])
                );
                drop(stream);
                time::sleep(Duration::from_millis(50)).await;
                assert!(!attach.stdin_closed().is_cancelled());
                Ok(())
            })
            .await
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

    /// Send a port forward request with the extra headers, return the
    /// response head and the connection.
    async fn port_forward_request(
        server: &StreamingServer,
        url: &str,
        headers: &str,
    ) -> Result<(String, TcpStream)> {
        let path = url
            .strip_prefix(&format!("http://{ADDR}:{}", server.port))
            .context("url")?;
        let mut stream = TcpStream::connect((ADDR, server.port)).await?;
        stream
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: {ADDR}\r\nContent-Length: 0\r\n{headers}\r\n"
                )
                .as_bytes(),
            )
            .await?;
        let mut response = vec![];
        while !response.ends_with(b"\r\n\r\n") {
            response.push(stream.read_u8().await?);
        }
        Ok((String::from_utf8_lossy(&response).to_lowercase(), stream))
    }

    const SPDY_UPGRADE: &str = "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n";

    #[tokio::test]
    async fn port_forward_upgrades_to_spdy() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                let url = server
                    .port_forward_url("/proc/self/ns/net".into(), None)
                    .await;

                let (response, stream) = port_forward_request(
                    &server,
                    &url,
                    &format!("{SPDY_UPGRADE}X-Stream-Protocol-Version: v4.channel.k8s.io, portforward.k8s.io\r\n"),
                )
                .await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                assert!(response.contains("upgrade: spdy/3.1"), "{response}");
                assert!(
                    response.contains("x-stream-protocol-version: portforward.k8s.io"),
                    "{response}"
                );

                // The connection speaks SPDY now, a stream without headers
                // gets rejected.
                let mut client = Client::new(stream)?;
                client.open(1, Headers::default()).await?;
                assert!(matches!(
                    client.frame().await?,
                    Some(Frame::RstStream { stream_id: 1, .. })
                ));

                // The token got used.
                let (response, _) = port_forward_request(&server, &url, SPDY_UPGRADE).await?;
                assert!(response.starts_with("http/1.1 404"), "{response}");
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_rejects_other_upgrades() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;

                for (headers, status) in [
                    (
                        "Connection: Upgrade\r\nUpgrade: websocket\r\n\
                         X-Stream-Protocol-Version: portforward.k8s.io\r\n"
                            .to_owned(),
                        "400",
                    ),
                    (SPDY_UPGRADE.to_owned(), "400"),
                    (
                        format!("{SPDY_UPGRADE}X-Stream-Protocol-Version: v4.channel.k8s.io\r\n"),
                        "403",
                    ),
                ] {
                    let url = server
                        .port_forward_url("/proc/self/ns/net".into(), None)
                        .await;
                    let (response, _) = port_forward_request(&server, &url, &headers).await?;
                    assert!(
                        response.starts_with(&format!("http/1.1 {status}")),
                        "{response}"
                    );
                    if status == "403" {
                        assert!(
                            response.contains(
                                "x-accepted-stream-protocol-versions: portforward.k8s.io"
                            ),
                            "{response}"
                        );
                    }
                }

                // Exec and attach tokens are no port forward tokens.
                let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
                let (_, child) = ReapableChild::from_child(Child {
                    id: "id".into(),
                    kind: ChildKind::Container,
                    pid: 0,
                    exit_paths: vec![],
                    oom_exit_paths: vec![],
                    timeout: None,
                    io,
                    cleanup_cmd: vec![],
                    token: CancellationToken::new(),
                });
                let url = server
                    .attach_url("id".into(), child, false, true, false, false)
                    .await
                    .replace(ATTACH_PATH, PORT_FORWARD_PATH);
                let (response, _) = port_forward_request(&server, &url, SPDY_UPGRADE).await?;
                assert!(response.starts_with("http/1.1 404"), "{response}");
                Ok(())
            })
            .await
    }

    /// Upgrade a port forward request to a WebSocket with the subprotocols
    /// and the query, return the response head and the connection.
    async fn port_forward_websocket(
        server: &StreamingServer,
        query: &str,
        protocols: &str,
    ) -> Result<(String, TcpStream)> {
        port_forward_websocket_with(server, query, protocols, None).await
    }

    async fn port_forward_websocket_with(
        server: &StreamingServer,
        query: &str,
        protocols: &str,
        idle_timeout: Option<Duration>,
    ) -> Result<(String, TcpStream)> {
        let url = server
            .port_forward_url("/proc/self/ns/net".into(), idle_timeout)
            .await;
        let path = url
            .strip_prefix(&format!("http://{ADDR}:{}", server.port))
            .context("url")?;
        let mut stream = TcpStream::connect((ADDR, server.port)).await?;
        let protocols = if protocols.is_empty() {
            String::new()
        } else {
            format!("Sec-WebSocket-Protocol: {protocols}\r\n")
        };
        stream
            .write_all(
                format!(
                    "GET {path}{query} HTTP/1.1\r\nHost: {ADDR}\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{protocols}\r\n"
                )
                .as_bytes(),
            )
            .await?;
        let mut response = vec![];
        while !response.ends_with(b"\r\n\r\n") {
            response.push(stream.read_u8().await?);
        }
        Ok((String::from_utf8_lossy(&response).to_lowercase(), stream))
    }

    #[tokio::test]
    async fn port_forward_over_websocket_channels() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;

                for base64 in [false, true] {
                    let listener = tokio::net::TcpListener::bind((ADDR, 0)).await?;
                    let port = listener.local_addr()?.port();
                    let protocol = if base64 {
                        "v4.base64.channel.k8s.io"
                    } else {
                        "v4.channel.k8s.io"
                    };
                    let (response, mut stream) =
                        port_forward_websocket(&server, &format!("?port={port}"), protocol).await?;
                    assert!(response.starts_with("http/1.1 101"), "{response}");
                    assert!(
                        response.contains(&format!("sec-websocket-protocol: {protocol}")),
                        "{response}"
                    );

                    // A message of the channel protocol, text for base64.
                    let message = |channel: u8, data: &[u8]| {
                        if base64 {
                            let mut text = vec![b'0' + channel];
                            text.extend(BASE64.encode(data).into_bytes());
                            (1, text)
                        } else {
                            (2, [&[channel], data].concat())
                        }
                    };

                    // The data and the error channel start with the port.
                    let prefix = port.to_le_bytes();
                    assert_eq!(server_frame(&mut stream).await?, message(0, &prefix));
                    assert_eq!(server_frame(&mut stream).await?, message(1, &prefix));

                    let (opcode, payload) = message(0, b"request");
                    stream.write_all(&client_frame(opcode, &payload)).await?;
                    let (mut connection, _) = listener.accept().await?;
                    let mut request = [0; 7];
                    connection.read_exact(&mut request).await?;
                    assert_eq!(&request, b"request");
                    connection.write_all(b"response").await?;
                    drop(connection);

                    assert_eq!(server_frame(&mut stream).await?, message(0, b"response"));
                    // The only port is done, which ends the session.
                    assert_eq!(server_frame(&mut stream).await?.0, 8);
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_ends_idle_sessions_with_stuck_ports() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                // A pod which accepts, but never reads.
                let listener = tokio::net::TcpListener::bind((ADDR, 0)).await?;
                let port = listener.local_addr()?.port();
                let accepted = tokio::spawn(async move { listener.accept().await });

                let (response, stream) = port_forward_websocket_with(
                    &server,
                    &format!("?port={port}"),
                    "",
                    Some(Duration::from_millis(500)),
                )
                .await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                let (mut reader, mut writer) = stream.into_split();
                server_frame(&mut reader).await?;
                server_frame(&mut reader).await?;
                let _connection = accepted.await??;

                // More than the socket buffers and the port queue hold.
                let flood = tokio::spawn(async move {
                    let frame = client_frame(2, &[&[0], &[0; 60_000][..]].concat());
                    while writer.write_all(&frame).await.is_ok() {}
                });
                let (opcode, _) = time::timeout(Duration::from_secs(10), server_frame(&mut reader))
                    .await
                    .context("no close of the idle session")??;
                assert_eq!(opcode, 8);
                flood.abort();
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_reports_errors() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                let port = tokio::net::TcpListener::bind((ADDR, 0))
                    .await?
                    .local_addr()?
                    .port();

                let (response, mut stream) =
                    port_forward_websocket(&server, &format!("?port={port}"), "").await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                server_frame(&mut stream).await?;
                server_frame(&mut stream).await?;

                let (opcode, payload) = server_frame(&mut stream).await?;
                assert_eq!((opcode, payload[0]), (2, 1));
                let error = String::from_utf8_lossy(&payload[1..]).into_owned();
                assert!(
                    error.starts_with(&format!(
                        "error forwarding port {port}: failed to connect to localhost:{port}"
                    )),
                    "{error}"
                );
                assert_eq!(server_frame(&mut stream).await?.0, 8);
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_rejects_invalid_requests() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                for (query, protocols, status) in [
                    ("", "v4.channel.k8s.io", "400"),
                    ("?port=0", "", "400"),
                    ("?port=80", "v5.channel.k8s.io", "403"),
                ] {
                    let (response, _) = port_forward_websocket(&server, query, protocols).await?;
                    assert!(
                        response.starts_with(&format!("http/1.1 {status}")),
                        "{response}"
                    );
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_tunnels_spdy() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                // The tunnel needs no ports, they are in the SPDY streams.
                let (response, mut stream) = port_forward_websocket(
                    &server,
                    "",
                    "v4.channel.k8s.io, SPDY/3.1+portforward.k8s.io",
                )
                .await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                assert!(
                    response.contains("sec-websocket-protocol: spdy/3.1+portforward.k8s.io"),
                    "{response}"
                );

                // A stream without headers, which gets rejected.
                let (mut client, mut encoded) = Client::pair()?;
                client.open(1, Headers::default()).await?;
                let mut syn_stream = vec![0; 1024];
                let len = encoded.read(&mut syn_stream).await?;
                stream
                    .write_all(&client_frame(2, &syn_stream[..len]))
                    .await?;

                let (opcode, payload) = server_frame(&mut stream).await?;
                assert_eq!(opcode, 2);
                assert_eq!(
                    payload,
                    [
                        0x80, 0x03, 0, 0x03, 0, 0, 0, 0x08, 0, 0, 0, 0x01, 0, 0, 0, 0x05
                    ]
                );
                Ok(())
            })
            .await
    }

    #[test]
    fn stdin_messages_are_dispatched_by_their_first_byte() {
        for (data, expected) in [
            (
                &[STDIN_BYTE, b'l', b's'][..],
                ControlFlow::Continue(Some("ls")),
            ),
            (&[STDIN_BYTE], ControlFlow::Continue(Some(""))),
            (&[CLOSE_BYTE], ControlFlow::Break(())),
            (&[CLOSE_BYTE, b'x'], ControlFlow::Break(())),
            // Unknown and empty messages are ignored.
            (&[42, b'x'], ControlFlow::Continue(None)),
            (&[], ControlFlow::Continue(None)),
        ] {
            let expected = expected.map_continue(|stdin: Option<&'static str>| {
                stdin.map(|stdin| Bytes::from_static(stdin.as_bytes()))
            });
            assert_eq!(
                StreamingServer::handle_stdin(Bytes::copy_from_slice(data)),
                expected,
                "{data:?}"
            );
        }
    }

    #[test]
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

    #[test]
    fn success_status_frame() -> Result<()> {
        assert_eq!(SUCCESS_FRAME[0], STREAM_ERR_BYTE);
        assert_eq!(
            SUCCESS_FRAME,
            b"\x03{\"metadata\":{},\"status\":\"Success\"}"
        );
        Ok(())
    }

    #[test]
    fn exit_code_error_message() -> Result<()> {
        assert_eq!(
            serde_json::to_string(&ErrorMessage::new(137))?,
            r#"{"status":"Failure","reason":"NonZeroExitCode","details":{"causes":[{"reason":"ExitCode","message":"137"}]},"message":"command terminated with non-zero exit code"}"#
        );
        Ok(())
    }

    #[test]
    fn resize_events_bypass_pending_stdin() {
        let mut pending = PendingStdin::default();
        let (resize_tx, resize_rx) = watch::channel(None);

        let mut read = |data: &'static [u8]| {
            StreamingServer::read_message(
                Message::Binary(Bytes::from_static(data)),
                &mut pending,
                &resize_tx,
            )
        };
        assert_eq!(read(&[STDIN_BYTE, b'a']), ControlFlow::Continue(()));
        assert_eq!(read(&[RESIZE_BYTE, b'1']), ControlFlow::Continue(()));
        assert_eq!(read(&[RESIZE_BYTE, b'2']), ControlFlow::Continue(()));

        // Only the latest size is kept, the stdin waits on its own.
        assert_eq!(
            resize_rx.borrow().as_deref(),
            Some(&[RESIZE_BYTE, b'2'][..])
        );
        assert_eq!(pending.pop(), Bytes::from_static(&[STDIN_BYTE, b'a']));
        assert!(pending.is_empty());

        let close = StreamingServer::read_message(Message::Close(None), &mut pending, &resize_tx);
        assert_eq!(close, ControlFlow::Break(()));
    }

    #[test]
    fn pending_stdin_is_bounded() {
        let mut pending = PendingStdin::default();
        pending.push(Bytes::from(vec![0; PENDING_STDIN_MAX_BYTES - 1]));
        assert!(!pending.is_full());
        pending.push(Bytes::from_static(b"x"));
        assert!(pending.is_full());

        assert_eq!(pending.pop().len(), PENDING_STDIN_MAX_BYTES - 1);
        assert!(!pending.is_full());
        pending.clear();
        assert!(pending.is_empty());
        assert_eq!(pending.bytes, 0);
    }
}
