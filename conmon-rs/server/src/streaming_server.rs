use crate::conmon_capnp::conmon::CgroupManager;
use crate::{
    attach::SharedContainerAttach,
    bounded_hashmap::BoundedHashMap,
    child::{Child, ChildKind},
    child_reaper::{ChildReaper, ExitChannelData, ReapableChild},
    config::Config,
    container_io::{
        ContainerIO, ContainerIOType, Drain, EarlyStdio, Message as IOMessage, Pipe,
        SharedContainerIO,
    },
    port_forward::{self, PortForward, WebSocketProtocol},
    process::Process,
    remotecommand::{
        ClientEvent, ClientInput, ExpectedStreams, Framing, Output, SPDY_PROTOCOLS, STDERR_BYTE,
        STDOUT_BYTE, SpdyStreams, Status, Stdin, Version, WebSocketInput, spdy_version,
    },
    server::GenerateRuntimeArgs,
    spdy::{self, Connection},
};
use anyhow::{Context, Result};
use async_channel::Receiver as IOReceiver;
use axum::{
    Router,
    body::Body,
    extract::{
        FromRequestParts, Path, Request, State as AxumState,
        ws::{WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderName, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use futures::stream::StreamExt;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use libc::pid_t;
use nix::{sys::signal::Signal, unistd::Pid};
use serde::Deserialize;
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

/// How often a SPDY session pings the client, to notice a client which went
/// away also while its stdin waits for the container.
const SPDY_PING_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    spdy::PING_INTERVAL
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
            // SPDY clients upgrade with a POST.
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

    /// Handle an exec or attach request, which the client upgrades to a
    /// WebSocket or SPDY.
    async fn handle(
        Path(token): Path<Uuid>,
        AxumState(state): AxumState<Arc<RwLock<State>>>,
        mut request: Request,
    ) -> Response {
        async move {
            // Only at debug level, a request which gets rejected leaves the
            // token valid, the log must not hand it out.
            debug!("Got request for token: {token}");
            // An unknown token comes first, the request cannot succeed.
            if !state.read().await.sessions.contains_key(&token) {
                error!("Unable to find session for token: {token}");
                return StatusCode::NOT_FOUND.into_response();
            }
            if Self::is_websocket_request(request.headers()) {
                let requested = Self::websocket_protocols(request.headers());
                let Some((framing, subprotocol)) =
                    Framing::negotiate(requested.iter().map(String::as_str))
                else {
                    return (
                        StatusCode::FORBIDDEN,
                        format!(
                            "unable to upgrade: requested protocol(s) are not supported: {requested:?}"
                        ),
                    )
                        .into_response();
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
                let Some(session) = Self::take_session(&state, token).await else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                return ws
                    .max_message_size(MAX_MESSAGE_SIZE)
                    .max_frame_size(MAX_MESSAGE_SIZE)
                    .on_upgrade(move |socket| {
                        Self::handle_websocket(socket, session, framing)
                            .instrument(debug_span!("websocket", %token))
                    });
            }

            let supported = SPDY_PROTOCOLS.map(|(protocol, _)| protocol);
            let protocol = match Self::spdy_handshake(request.headers(), &supported) {
                Ok(protocol) => protocol,
                Err(response) => return *response,
            };
            let version = spdy_version(protocol).unwrap_or(Version::V1);
            let Some(on_upgrade) = request.extensions_mut().remove::<OnUpgrade>() else {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "unable to upgrade: connection does not support upgrades",
                )
                    .into_response();
            };
            let Some(session) = Self::take_session(&state, token).await else {
                return StatusCode::NOT_FOUND.into_response();
            };
            task::spawn(
                async move {
                    match on_upgrade.await {
                        Ok(upgraded) => {
                            Self::handle_spdy(TokioIo::new(upgraded), session, version).await
                        }
                        Err(e) => error!("Unable to upgrade connection: {e}"),
                    }
                }
                .instrument(debug_span!("spdy", %token)),
            );
            Self::switching_protocols(protocol)
        }
        .instrument(debug_span!("handle_common", %token))
        .await
    }

    /// Take the exec or attach session of a valid request. Requests which get
    /// rejected before leave the token, so that the client can retry.
    async fn take_session(state: &RwLock<State>, token: Uuid) -> Option<Session> {
        let Some(session) = state.write().await.sessions.remove(&token) else {
            error!("Unable to find session for token: {token}");
            return None;
        };
        // Only what identifies the session, the command of an exec may carry
        // secrets.
        info!(
            "Got valid {} session for container {} and token {token}",
            session.typ(),
            session.container_id()
        );
        Some(session)
    }

    /// The response of a successful SPDY upgrade.
    fn switching_protocols(protocol: &'static str) -> Response {
        Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, SPDY_31)
            .header(PROTOCOL_VERSION_HEADER, protocol)
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }

    /// The WebSocket subprotocols a client requested.
    fn websocket_protocols(headers: &HeaderMap) -> Vec<String> {
        headers
            .get_all(header::SEC_WEBSOCKET_PROTOCOL)
            .into_iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// Handle a port forward request, which the client upgrades to SPDY.
    async fn handle_port_forward(
        Path(token): Path<Uuid>,
        AxumState(state): AxumState<Arc<RwLock<State>>>,
        mut request: Request,
    ) -> Response {
        async move {
            debug!("Got port forward request for token: {token}");
            // An unknown token comes first, the request cannot succeed.
            if !state.read().await.port_forwards.contains_key(&token) {
                error!("Unable to find port forward session for token: {token}");
                return StatusCode::NOT_FOUND.into_response();
            }
            if Self::is_websocket_request(request.headers()) {
                return Self::upgrade_port_forward_websocket(token, &state, request).await;
            }
            let protocol = match Self::spdy_handshake(request.headers(), &[port_forward::PROTOCOL])
            {
                Ok(protocol) => protocol,
                Err(response) => return *response,
            };
            let Some(on_upgrade) = request.extensions_mut().remove::<OnUpgrade>() else {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "unable to upgrade: connection does not support upgrades",
                )
                    .into_response();
            };
            let Some(session) = Self::take_port_forward(&state, token).await else {
                return StatusCode::NOT_FOUND.into_response();
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
            Self::switching_protocols(protocol)
        }
        .instrument(debug_span!("handle_port_forward", %token))
        .await
    }

    /// Take the session of a valid request. Requests which get rejected
    /// before leave the token, so that the client can retry. Another request
    /// may have taken it meanwhile.
    async fn take_port_forward(state: &RwLock<State>, token: Uuid) -> Option<PortForward> {
        let session = state.write().await.port_forwards.remove(&token);
        if session.is_none() {
            debug!(
                "Port forward session for token {token} got taken by another request or expired"
            );
        }
        session
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
        state: &RwLock<State>,
        request: Request,
    ) -> Response {
        let requested = Self::websocket_protocols(request.headers());
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
        let Some(session) = Self::take_port_forward(state, token).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        ws.max_message_size(MAX_MESSAGE_SIZE)
            .max_frame_size(MAX_MESSAGE_SIZE)
            .on_upgrade(move |socket| {
                session
                    .serve_websocket(socket, protocol, ports)
                    .instrument(debug_span!("port_forward", %token))
            })
    }

    /// Check that the request asks for a SPDY/3.1 upgrade and negotiate one
    /// of the supported protocols, by the order of the client. The rejections
    /// are the ones of the SPDY server of Kubernetes.
    fn spdy_handshake(
        headers: &HeaderMap,
        supported: &[&'static str],
    ) -> Result<&'static str, Box<Response>> {
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
            return Err(Box::new(
                (
                    StatusCode::BAD_REQUEST,
                    "unable to upgrade: missing WebSocket or SPDY/3.1 upgrade headers in request",
                )
                    .into_response(),
            ));
        }

        let protocols: Vec<_> = values(PROTOCOL_VERSION_HEADER)
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if protocols.is_empty() {
            return Err(Box::new(
                (
                    StatusCode::BAD_REQUEST,
                    format!("unable to upgrade: header {PROTOCOL_VERSION_HEADER} is required"),
                )
                    .into_response(),
            ));
        }
        protocols
            .iter()
            .find_map(|protocol| supported.iter().find(|s| *s == protocol).copied())
            .ok_or_else(|| {
                let mut response = (
                    StatusCode::FORBIDDEN,
                    format!(
                        "unable to upgrade: unable to negotiate protocol: client supports {protocols:?}, server accepts {supported:?}"
                    ),
                )
                    .into_response();
                for protocol in supported {
                    response.headers_mut().append(
                        ACCEPTED_PROTOCOL_VERSIONS_HEADER,
                        header::HeaderValue::from_static(protocol),
                    );
                }
                Box::new(response)
            })
    }

    /// Handle a single websocket connection.
    async fn handle_websocket(socket: WebSocket, session: Session, framing: Framing) {
        let (sender, messages) = socket.split();
        let mut output = Output::websocket(sender, framing);
        // A client which went away already ends the session in its loop,
        // which tears it down.
        if let Err(e) = output.announce(session.stdout(), session.stderr()).await {
            debug!("Unable to announce the session: {e:#}");
        }
        let input = WebSocketInput { messages, framing };
        Self::run_session(output, input, session).await;
        info!("Closing websocket connection");
    }

    /// Handle a connection which got upgraded to SPDY: wait for the streams of
    /// the client, then run the session on them.
    async fn handle_spdy<T>(io: T, session: Session, version: Version)
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let terminal = session.terminal().await;
        // A terminal merges stderr into stdout, Kubernetes clients do not
        // open a stream for it then.
        let expected = ExpectedStreams {
            stdin: session.stdin(),
            stdout: session.stdout(),
            stderr: session.stderr() && !terminal,
        };
        let mut connection =
            Connection::serve(io, None, Some(SPDY_PING_INTERVAL), move |headers| {
                SpdyStreams::accept(headers, version)
            });
        let streams = match SpdyStreams::wait(&mut connection, expected).await {
            Ok(streams) => streams,
            Err(e) => {
                error!("Unable to start the session: {e:#}");
                connection.abort();
                return;
            }
        };
        let (input, output) = streams.into_parts(connection, version).await;
        Self::run_session(Output::spdy(output), input, session).await;
        info!("Closing SPDY connection");
    }

    /// Run a session until it ended or the client went away.
    async fn run_session(output: Output, input: impl ClientInput, session: Session) {
        let peer_closed = output.peer_closed();
        let (stdin_tx, stdin_rx) = mpsc::channel(16);
        // Resize events go through their own channel, so that they do not
        // queue behind stdin the container does not read. Only the latest
        // size matters.
        let (resize_tx, resize_rx) = watch::channel(None);

        let mut send_task = Self::write_task(output, stdin_rx, resize_rx, session);
        let mut recv_task = Self::read_task(input, stdin_tx, resize_tx, peer_closed);

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
    }

    /// Build a common write task based on the session type.
    fn write_task(
        sender: Output,
        stdin_rx: MpscReceiver<Stdin>,
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
                if let Err(e) = sender
                    .status(&Status::Error(format!("{typ} failed: {e:#}")))
                    .await
                {
                    error!("Unable to send error message: {e:#}");
                }
            }

            sender.close().await;
        })
    }

    /// Build a common read task.
    fn read_task(
        mut input: impl ClientInput,
        stdin_tx: MpscSender<Stdin>,
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
                    event = input.next(), if !pending.is_full() => match event {
                        Some(event) => {
                            if Self::read_event(event, &mut pending, &resize_tx).is_break() {
                                peer_closed.store(true, Ordering::Release);
                                closed = true;
                                break;
                            }
                        }
                        None => break,
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

    /// Handle a single event of the client and return the control flow
    /// decision.
    fn read_event(
        event: ClientEvent,
        pending: &mut PendingStdin,
        resize_tx: &watch::Sender<Option<Bytes>>,
    ) -> ControlFlow<(), ()> {
        match event {
            ClientEvent::Stdin(stdin) => pending.push(stdin),
            ClientEvent::Resize(data) => {
                resize_tx.send_replace(Some(data));
            }
            ClientEvent::Closed => return ControlFlow::Break(()),
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
        mut stdin_rx: MpscReceiver<Stdin>,
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
                    sender.ping().await?
                },

                res = stdin_rx.recv(), if pending_stdin.is_none() => match res {
                    // The client closed the connection, everything it sent
                    // reached the process.
                    None => break,
                    Some(_) if !stdin_enabled || stdin_closed => {}
                    Some(stdin) => match stdin {
                        // The client closed its stdin. Close the process stdin
                        // as well and keep forwarding the output until it
                        // exits. A terminal cannot be closed without ending
                        // its output, it gets its end of file character
                        // instead, like from a user pressing Ctrl-D. A process
                        // reading lines sees the end of its input then, one
                        // after a line without its ending needs a second one,
                        // like a user would type it.
                        Stdin::Close => {
                            stdin_closed = true;
                            match io.terminal_eof().await {
                                Some(eof) => pending_stdin = Some(Bytes::from(vec![eof])),
                                None => {
                                    attach.close_container_stdin();
                                }
                            }
                        }
                        Stdin::Data(data) => pending_stdin = Some(data),
                    },
                },

                Ok(exit_data) = exit_rx.recv() => {
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
                    sender.status(&Status::exited(exit_data.exit_code)).await?;
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
        mut stdin_rx: MpscReceiver<Stdin>,
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
                    sender.ping().await?
                },

                res = stdin_rx.recv(), if pending_stdin.is_none() => match res {
                    // The client closed the connection, everything it sent
                    // reached the container. Nobody gets a status anymore.
                    None => return Ok(()),
                    Some(_) if stdin_closed => {}
                    Some(stdin) => match stdin {
                        // The client closed its stdin. Without a terminal and
                        // stdin once, that ends the session, like for CRI-O
                        // with conmon. Otherwise keep forwarding the output
                        // until it ends, and with stdin once close the
                        // container stdin.
                        Stdin::Close if !terminal && !stop_after_stdin_eof => break,
                        Stdin::Close => {
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
                        Stdin::Data(data) => pending_stdin = Some(data),
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

        sender.status(&Status::Success).await?;
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
                sender.data(STDOUT_BYTE, &data).await
            }
            IOMessage::Data(data, Pipe::StdErr) if stderr_enabled => {
                sender.data(STDERR_BYTE, &data).await
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
                    sender.data(stream_byte, &data).await?;
                }
                Some(Ok(IOMessage::Done)) | Some(Err(_)) | None => return Ok(()),
            }
        }
    }

    /// Handle a single resize event of the client, its JSON.
    async fn handle_resize(data: &[u8], io: &SharedContainerIO) -> Result<()> {
        let e = serde_json::from_slice::<ResizeEvent>(data).context("unmarshal resize event")?;
        trace!("Got resize message: {e:?}");
        // Clients send resize events regardless of a terminal, they are no
        // reason to end the session without one.
        if !io.try_resize(e.width, e.height).await? {
            debug!("Ignoring resize event, container has no terminal");
        }
        Ok(())
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

    fn stdin(&self) -> bool {
        match self {
            Session::Exec(s) => s.stdin,
            Session::Attach(s) => s.stdin,
        }
    }

    fn stdout(&self) -> bool {
        match self {
            Session::Exec(s) => s.stdout,
            Session::Attach(s) => s.stdout,
        }
    }

    fn stderr(&self) -> bool {
        match self {
            Session::Exec(s) => s.stderr,
            Session::Attach(s) => s.stderr,
        }
    }

    /// Whether the process of the session has a terminal.
    async fn terminal(&self) -> bool {
        match self {
            Session::Exec(s) => matches!(s.container_io.typ(), ContainerIOType::Terminal(_)),
            Session::Attach(s) => s.child.io().terminal_eof().await.is_some(),
        }
    }
}

/// Stdin of a client which the container did not take yet, bounded by
/// [`PENDING_STDIN_MAX_BYTES`].
#[derive(Debug, Default)]
struct PendingStdin {
    queue: VecDeque<Stdin>,
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

    fn push(&mut self, stdin: Stdin) {
        self.bytes += stdin.len();
        self.queue.push_back(stdin);
    }

    /// The oldest message, callers check that there is one.
    fn pop(&mut self) -> Stdin {
        let stdin = self.queue.pop_front().unwrap_or(Stdin::Close);
        self.bytes -= stdin.len();
        stdin
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
        remotecommand::{CLOSE_BYTE, STDIN_BYTE, STREAM_ERR_BYTE},
        spdy::testing::{Client, Frame, Headers},
    };
    use anyhow::bail;
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        task::LocalSet,
    };

    const PROTOCOL_V5: &str = "v5.channel.k8s.io";

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
        let response = response_head(&mut stream).await?;
        assert!(response.starts_with("http/1.1 101"), "{response}");
        Ok((io, server, stream))
    }

    /// Read the head of an HTTP response, in lowercase.
    async fn response_head(stream: &mut TcpStream) -> Result<String> {
        let mut response = vec![];
        while !response.ends_with(b"\r\n\r\n") {
            response.push(stream.read_u8().await?);
        }
        Ok(String::from_utf8_lossy(&response).to_lowercase())
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
                stream
                    .write_all(&client_frame(0x2, &[CLOSE_BYTE, STDIN_BYTE]))
                    .await?;

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
                frames.extend(client_frame(0x2, &[CLOSE_BYTE, STDIN_BYTE]));
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
                stream
                    .write_all(&client_frame(0x2, &[CLOSE_BYTE, STDIN_BYTE]))
                    .await?;

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
        StreamingServer::handle_resize(br#"{"Width":80,"Height":24}"#, &io).await?;

        // Garbage is still an error.
        assert!(StreamingServer::handle_resize(b"x", &io).await.is_err());
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
        Ok((response_head(&mut stream).await?, stream))
    }

    const SPDY_UPGRADE: &str = "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n";

    /// A valid SPDY port forward upgrade.
    const SPDY_PORT_FORWARD: &str = "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                                     X-Stream-Protocol-Version: portforward.k8s.io\r\n";

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

                // The token got used, also a request with bad headers learns
                // that first.
                let (response, _) = port_forward_request(&server, &url, SPDY_UPGRADE).await?;
                assert!(response.starts_with("http/1.1 404"), "{response}");
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_tokens_survive_rejected_requests() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;

                // A SPDY request without the protocol, then a valid one.
                let url = server
                    .port_forward_url("/proc/self/ns/net".into(), None)
                    .await;
                let (response, _) = port_forward_request(&server, &url, SPDY_UPGRADE).await?;
                assert!(response.starts_with("http/1.1 400"), "{response}");
                let (response, _) = port_forward_request(&server, &url, SPDY_PORT_FORWARD).await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");

                // A WebSocket request without ports, then a valid one.
                let url = server
                    .port_forward_url("/proc/self/ns/net".into(), None)
                    .await;
                let (response, _) = port_forward_websocket_to(&server, &url, "", "").await?;
                assert!(response.starts_with("http/1.1 400"), "{response}");
                let (response, _) =
                    port_forward_websocket_to(&server, &url, "?port=80", "").await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
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
                let (response, _) = port_forward_request(&server, &url, SPDY_PORT_FORWARD).await?;
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
        port_forward_websocket_to(server, &url, query, protocols).await
    }

    /// Upgrade a request for the URL to a WebSocket.
    async fn port_forward_websocket_to(
        server: &StreamingServer,
        url: &str,
        query: &str,
        protocols: &str,
    ) -> Result<(String, TcpStream)> {
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
        Ok((response_head(&mut stream).await?, stream))
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

    /// A container and its child, the token ends the container.
    fn attach_child(
        terminal: bool,
    ) -> Result<(SharedContainerIO, ReapableChild, CancellationToken)> {
        let io = SharedContainerIO::new(ContainerIO::new(terminal, ContainerLog::new())?);
        let token = CancellationToken::new();
        let (_, child) = ReapableChild::from_child(Child {
            id: "id".into(),
            kind: ChildKind::Container,
            pid: 0,
            exit_paths: vec![],
            oom_exit_paths: vec![],
            timeout: None,
            io: io.clone(),
            cleanup_cmd: vec![],
            token: token.clone(),
        });
        Ok((io, child, token))
    }

    /// Send an upgrade request for the URL, return the response head and the
    /// connection.
    async fn upgrade(
        server: &StreamingServer,
        url: &str,
        method: &str,
        headers: &str,
    ) -> Result<(String, TcpStream)> {
        let path = url
            .strip_prefix(&format!("http://{ADDR}:{}", server.port))
            .context("url")?;
        let mut stream = TcpStream::connect((ADDR, server.port)).await?;
        stream
            .write_all(
                format!(
                    "{method} {path} HTTP/1.1\r\nHost: {ADDR}\r\nContent-Length: 0\r\n{headers}\r\n"
                )
                .as_bytes(),
            )
            .await?;
        Ok((response_head(&mut stream).await?, stream))
    }

    const SPDY_EXEC_UPGRADE: &str = "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
        X-Stream-Protocol-Version: v5.channel.k8s.io,v4.channel.k8s.io,v3.channel.k8s.io\r\n";

    /// Read SPDY frames until one matches, return it.
    async fn spdy_frame_until(
        client: &mut Client<TcpStream>,
        matches: impl Fn(&Frame) -> bool,
    ) -> Result<Frame> {
        loop {
            match client.frame().await? {
                Some(frame) if matches(&frame) => return Ok(frame),
                Some(_) => {}
                None => bail!("connection ended"),
            }
        }
    }

    /// An attach session over SPDY with the streams of a client-go v4 client:
    /// error 1, stdin 3 and stdout 5.
    async fn spdy_attach(
        stop_after_stdin_eof: bool,
    ) -> Result<(SharedContainerIO, StreamingServer, Client<TcpStream>)> {
        spdy_attach_with(false, stop_after_stdin_eof).await
    }

    /// Like [`spdy_attach`], the container may have a terminal, which the
    /// client does not ask for.
    async fn spdy_attach_with(
        terminal: bool,
        stop_after_stdin_eof: bool,
    ) -> Result<(SharedContainerIO, StreamingServer, Client<TcpStream>)> {
        let (io, child, _token) = attach_child(terminal)?;
        let mut server = StreamingServer::default();
        server.start_if_required().await?;
        let url = server
            .attach_url("id".into(), child, true, true, false, stop_after_stdin_eof)
            .await;
        let (response, stream) = upgrade(&server, &url, "POST", SPDY_EXEC_UPGRADE).await?;
        assert!(response.starts_with("http/1.1 101"), "{response}");
        // The server does not speak v5 over SPDY.
        assert!(
            response.contains("x-stream-protocol-version: v4.channel.k8s.io"),
            "{response}"
        );

        let mut client = Client::new(stream)?;
        for (stream_id, typ) in [(1, "error"), (3, "stdin"), (5, "stdout")] {
            client.open(stream_id, [("streamType", typ)].into()).await?;
        }
        Ok((io, server, client))
    }

    #[tokio::test]
    async fn spdy_attach_forwards_stdin_and_output() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut client) = spdy_attach(false).await?;
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: Bytes::from_static(b"in"),
                    })
                    .await?;
                let mut attach = io.attach().await;
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(data, b"in".as_slice());

                attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                let frame = spdy_frame_until(&mut client, |f| {
                    matches!(f, Frame::Data { stream_id: 5, .. })
                })
                .await?;
                assert_eq!(
                    frame,
                    Frame::Data {
                        stream_id: 5,
                        fin: false,
                        data: Bytes::from_static(b"out")
                    }
                );
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn spdy_attach_ends_with_success_on_stdin_close() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut client) = spdy_attach(false).await?;
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: true,
                        data: Bytes::new(),
                    })
                    .await?;

                let status = spdy_frame_until(
                    &mut client,
                    |f| matches!(f, Frame::Data { stream_id: 1, data, .. } if !data.is_empty()),
                )
                .await?;
                assert_eq!(
                    status,
                    Frame::Data {
                        stream_id: 1,
                        fin: false,
                        data: Bytes::from_static(br#"{"metadata":{},"status":"Success"}"#)
                    }
                );
                // The output and the error stream end, then the connection.
                let mut ended = vec![];
                loop {
                    match client.frame().await? {
                        Some(Frame::Data {
                            stream_id,
                            fin: true,
                            ..
                        }) => ended.push(stream_id),
                        Some(_) => {}
                        None => break,
                    }
                }
                ended.sort();
                // Stdin ends only with the session.
                assert_eq!(ended, [1, 3, 5]);
                // Without stdin once, the container stdin stays open.
                assert!(!io.attach().await.stdin_closed().is_cancelled());
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn spdy_stdin_stays_open_until_the_end() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut client) = spdy_attach(false).await?;
                // Client-go takes the end of the stdin stream of the server as
                // the end of stdin.
                let early = async {
                    loop {
                        if let Some(Frame::Data {
                            stream_id: 3,
                            fin: true,
                            ..
                        }) = client.frame().await?
                        {
                            return anyhow::Ok(());
                        }
                    }
                };
                assert!(
                    time::timeout(Duration::from_millis(300), early)
                        .await
                        .is_err()
                );

                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: Bytes::from_static(b"in"),
                    })
                    .await?;
                let mut attach = io.attach().await;
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(data, b"in".as_slice());
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn spdy_attach_to_a_terminal_without_tty() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                // The client does not ask for a terminal, it opens no resize
                // stream, the session starts nevertheless.
                let (io, _server, mut client) = spdy_attach_with(true, false).await?;
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: Bytes::from_static(b"in"),
                    })
                    .await?;
                let mut attach = io.attach().await;
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(data, b"in".as_slice());

                // A resize stream may still come, a second one not.
                client.open(7, [("streamType", "resize")].into()).await?;
                client.open(9, [("streamType", "resize")].into()).await?;
                let reset =
                    spdy_frame_until(&mut client, |f| matches!(f, Frame::RstStream { .. })).await?;
                assert!(
                    matches!(reset, Frame::RstStream { stream_id: 9, .. }),
                    "{reset:?}"
                );
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn late_output_streams_end_instead_of_failing() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                // A container with a terminal, the client asks for stdin
                // without a terminal and opens stderr after the session
                // started, also for versions without resize streams.
                for protocol in ["v2.channel.k8s.io", "v4.channel.k8s.io"] {
                    let (io, child, _token) = attach_child(true)?;
                    let mut server = StreamingServer::default();
                    server.start_if_required().await?;
                    let url = server
                        .attach_url("id".into(), child, true, true, true, false)
                        .await;
                    let (response, stream) = upgrade(
                        &server,
                        &url,
                        "POST",
                        &format!(
                            "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                             X-Stream-Protocol-Version: {protocol}\r\n"
                        ),
                    )
                    .await?;
                    assert!(response.starts_with("http/1.1 101"), "{response}");
                    let mut client = Client::new(stream)?;
                    for (stream_id, typ) in [(1, "error"), (3, "stdin"), (5, "stdout")] {
                        client.open(stream_id, [("streamType", typ)].into()).await?;
                    }
                    client
                        .send(Frame::Data {
                            stream_id: 3,
                            fin: false,
                            data: Bytes::from_static(b"in"),
                        })
                        .await?;
                    let mut attach = io.attach().await;
                    let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                    assert_eq!(data, b"in".as_slice());

                    client.open(7, [("streamType", "stderr")].into()).await?;
                    let frame = spdy_frame_until(&mut client, |f| {
                        matches!(
                            f,
                            Frame::RstStream { stream_id: 7, .. }
                                | Frame::Data {
                                    stream_id: 7,
                                    fin: true,
                                    ..
                                }
                        )
                    })
                    .await?;
                    assert!(matches!(frame, Frame::Data { .. }), "{protocol}: {frame:?}");
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn websocket_attach_closes_stdin_once_after_an_early_disconnect() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, stream, _token, response) =
                    websocket_attach_with("channel.k8s.io", true).await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                let attach = io.attach().await;
                // Before the client reads the announce of the session.
                drop(stream);
                time::timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn spdy_attach_closes_stdin_once() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut client) = spdy_attach(true).await?;
                let attach = io.attach().await;
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: true,
                        data: Bytes::new(),
                    })
                    .await?;
                time::timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn spdy_attach_ends_on_disconnect_while_stdin_waits() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut client) = spdy_attach(true).await?;
                let attach = io.attach().await;

                // More stdin than the session holds while nothing reads it,
                // so the stream queue fills and the connection is not read
                // anymore, until the client cannot write anymore either.
                let frame = Bytes::from(vec![b'x'; 32 * 1024]);
                let fill = async {
                    loop {
                        client
                            .send(Frame::Data {
                                stream_id: 3,
                                fin: false,
                                data: frame.clone(),
                            })
                            .await?;
                    }
                    #[allow(unreachable_code)]
                    anyhow::Ok(())
                };
                assert!(time::timeout(Duration::from_secs(1), fill).await.is_err());
                assert!(!attach.stdin_closed().is_cancelled());
                drop(client);

                // The pings of the session notice and end it, which closes
                // the stdin once.
                time::timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn spdy_streams_after_the_start_get_refused() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut client) = spdy_attach(false).await?;
                // A second stdin stream once the session started.
                client.open(7, [("streamType", "stdin")].into()).await?;
                spdy_frame_until(&mut client, |f| {
                    matches!(f, Frame::RstStream { stream_id: 7, .. })
                })
                .await?;

                // Its data does not hold up the session.
                for _ in 0..32 {
                    client
                        .send(Frame::Data {
                            stream_id: 7,
                            fin: false,
                            data: Bytes::from_static(b"late"),
                        })
                        .await?;
                }
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: Bytes::from_static(b"in"),
                    })
                    .await?;
                let mut attach = io.attach().await;
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(data, b"in".as_slice());
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn spdy_upgrades_get_negotiated() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                for (headers, status) in [
                    ("Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n", "400"),
                    (
                        "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                         X-Stream-Protocol-Version: v5.channel.k8s.io\r\n",
                        "403",
                    ),
                    ("", "400"),
                ] {
                    let (_, child, _token) = attach_child(false)?;
                    let url = server
                        .attach_url("id".into(), child, false, true, false, false)
                        .await;
                    let (response, _) = upgrade(&server, &url, "POST", headers).await?;
                    assert!(
                        response.starts_with(&format!("http/1.1 {status}")),
                        "{response}"
                    );
                    if status == "403" {
                        for protocol in SPDY_PROTOCOLS.map(|(p, _)| p) {
                            assert!(
                                response.contains(&format!(
                                    "x-accepted-stream-protocol-versions: {protocol}"
                                )),
                                "{response}"
                            );
                        }
                    }
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn rejected_upgrades_keep_the_token() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                let websocket = |protocol: &str| {
                    format!(
                        "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: {protocol}\r\n"
                    )
                };
                // An unknown token comes before the protocol.
                let (_, child, _token) = attach_child(false)?;
                let url = server
                    .attach_url("id".into(), child, false, true, false, false)
                    .await;
                let unknown = url.replace(
                    url.rsplit('/').next().context("token")?,
                    &Uuid::new_v4().to_string(),
                );
                let (response, _) = upgrade(&server, &unknown, "POST", "").await?;
                assert!(response.starts_with("http/1.1 404"), "{response}");

                for (rejected, status, valid) in [
                    (
                        "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                         X-Stream-Protocol-Version: v5.channel.k8s.io\r\n"
                            .to_owned(),
                        "403",
                        SPDY_EXEC_UPGRADE.to_owned(),
                    ),
                    ("".to_owned(), "400", SPDY_EXEC_UPGRADE.to_owned()),
                    (
                        websocket("v3.channel.k8s.io"),
                        "403",
                        websocket(PROTOCOL_V5),
                    ),
                ] {
                    let (_, child, _token) = attach_child(false)?;
                    let url = server
                        .attach_url("id".into(), child, false, true, false, false)
                        .await;
                    let (response, _) = upgrade(&server, &url, "POST", &rejected).await?;
                    assert!(response.starts_with(&format!("http/1.1 {status}")), "{response}");

                    // The client can retry with a request the server takes,
                    // which uses the token.
                    let method = if valid.contains("websocket") { "GET" } else { "POST" };
                    let (response, _) = upgrade(&server, &url, method, &valid).await?;
                    assert!(response.starts_with("http/1.1 101"), "{response}");
                    let (response, _) = upgrade(&server, &url, method, &valid).await?;
                    assert!(response.starts_with("http/1.1 404"), "{response}");
                }
                Ok(())
            })
            .await
    }

    /// An attach session over a WebSocket with the subprotocol, the token
    /// ends the container.
    async fn websocket_attach(
        protocol: &str,
    ) -> Result<(
        SharedContainerIO,
        StreamingServer,
        TcpStream,
        CancellationToken,
        String,
    )> {
        websocket_attach_with(protocol, false).await
    }

    /// Like [`websocket_attach`], with stdin once.
    async fn websocket_attach_with(
        protocol: &str,
        stop_after_stdin_eof: bool,
    ) -> Result<(
        SharedContainerIO,
        StreamingServer,
        TcpStream,
        CancellationToken,
        String,
    )> {
        let (io, child, token) = attach_child(false)?;
        let mut server = StreamingServer::default();
        server.start_if_required().await?;
        let url = server
            .attach_url("id".into(), child, true, true, false, stop_after_stdin_eof)
            .await;
        let (response, stream) = upgrade(
            &server,
            &url,
            "GET",
            &format!(
                "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: {protocol}\r\n"
            ),
        )
        .await?;
        Ok((io, server, stream, token, response))
    }

    #[tokio::test]
    async fn base64_websocket_attach() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut stream, token, response) =
                    websocket_attach("v4.base64.channel.k8s.io").await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                assert!(
                    response.contains("sec-websocket-protocol: v4.base64.channel.k8s.io"),
                    "{response}"
                );
                // An empty message on stdout tells the client that the
                // session started.
                assert_eq!(server_frame(&mut stream).await?, (0x1, b"1".to_vec()));

                let stdin = format!("0{}", BASE64.encode("in"));
                stream
                    .write_all(&client_frame(0x1, stdin.as_bytes()))
                    .await?;
                let mut attach = io.attach().await;
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(data, b"in".as_slice());

                attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                assert_eq!(
                    server_frame(&mut stream).await?,
                    (0x1, format!("1{}", BASE64.encode("out")).into_bytes())
                );

                // The v4 status is JSON, encoded like the output.
                token.cancel();
                assert_eq!(
                    server_frame(&mut stream).await?,
                    (
                        0x1,
                        format!(
                            "3{}",
                            BASE64.encode(br#"{"metadata":{},"status":"Success"}"#)
                        )
                        .into_bytes()
                    )
                );
                assert_eq!(server_frame(&mut stream).await?.0, 0x8);
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn v1_websocket_attach() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut stream, token, response) =
                    websocket_attach("channel.k8s.io").await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                assert_eq!(server_frame(&mut stream).await?, (0x2, vec![STDOUT_BYTE]));

                // There is no close signal before v5, the session goes on.
                stream
                    .write_all(&client_frame(0x2, &[CLOSE_BYTE, STDIN_BYTE]))
                    .await?;
                let attach = io.attach().await;
                attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                assert_eq!(server_frame(&mut stream).await?, (0x2, b"\x01out".to_vec()));

                // A success has no status before v4.
                token.cancel();
                assert_eq!(server_frame(&mut stream).await?.0, 0x8);
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn unsupported_websocket_protocols_get_rejected() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (_, _server, _, _, response) = websocket_attach("v3.channel.k8s.io").await?;
                assert!(response.starts_with("http/1.1 403"), "{response}");
                Ok(())
            })
            .await
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
    fn resize_events_bypass_pending_stdin() {
        let mut pending = PendingStdin::default();
        let (resize_tx, resize_rx) = watch::channel(None);

        let mut read = |event| StreamingServer::read_event(event, &mut pending, &resize_tx);
        let stdin = || ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"a")));
        assert_eq!(read(stdin()), ControlFlow::Continue(()));
        assert_eq!(
            read(ClientEvent::Resize(Bytes::from_static(b"1"))),
            ControlFlow::Continue(())
        );
        assert_eq!(
            read(ClientEvent::Resize(Bytes::from_static(b"2"))),
            ControlFlow::Continue(())
        );

        // Only the latest size is kept, the stdin waits on its own.
        assert_eq!(resize_rx.borrow().as_deref(), Some(&b"2"[..]));
        assert_eq!(pending.pop(), Stdin::Data(Bytes::from_static(b"a")));
        assert!(pending.is_empty());

        let close = StreamingServer::read_event(ClientEvent::Closed, &mut pending, &resize_tx);
        assert_eq!(close, ControlFlow::Break(()));
    }

    #[test]
    fn pending_stdin_is_bounded() {
        let mut pending = PendingStdin::default();
        pending.push(Stdin::Data(Bytes::from(vec![
            0;
            PENDING_STDIN_MAX_BYTES - 1
        ])));
        assert!(!pending.is_full());
        pending.push(Stdin::Close);
        assert!(!pending.is_full());
        pending.push(Stdin::Data(Bytes::from_static(b"x")));
        assert!(pending.is_full());

        assert_eq!(pending.pop().len(), PENDING_STDIN_MAX_BYTES - 1);
        assert!(!pending.is_full());
        pending.clear();
        assert!(pending.is_empty());
        assert_eq!(pending.bytes, 0);
    }
}
