//! The streaming server of exec, attach and port forward sessions: the URLs
//! with their one time tokens and the HTTP server their clients connect to.

use crate::{listener, mutex::lock, random_id::RandomId};
use anyhow::{Context, Result, bail};
use bounded_map::BoundedMap;
use hyper::{
    Method, Request, StatusCode,
    body::Incoming,
    header::{self, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use port_forward::PortForward;
use resources::Resources;
pub(crate) use resources::{DEFAULT_BUFFER_MIB, DEFAULT_FORWARD_CONNECTIONS};
use std::{
    convert::Infallible,
    io,
    net::Ipv4Addr,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
};
use stream_protocol::ClientStr;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OnceCell, OwnedSemaphorePermit, Semaphore},
    task,
    time::{self, Duration},
};
use tracing::{Level, debug, info, trace};
use web::Response;

mod attach;
mod bounded_map;
mod budget;
mod channel;
mod exec;
mod handshake;
mod idle;
mod port_forward;
mod remotecommand;
mod resources;
mod session;
mod spdy;
mod stream_protocol;
#[cfg(test)]
mod testing;
mod web;

pub(crate) use attach::AttachSession;
#[cfg(test)]
pub(crate) use bounded_map::DEFAULT_CAPACITY;
pub(crate) use exec::ExecSession;

/// The entry points of the fuzz targets.
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) mod fuzzing {
    pub use super::{
        channel::fuzzing::channel as channel_decode,
        handshake::fuzzing::upgrade_headers,
        port_forward::fuzzing::{query_ports as port_forward_query, spdy as port_forward_spdy},
        remotecommand::fuzzing::{resize_events, spdy as remotecommand_spdy},
        session::fuzzing::{client_messages, pending_stdin},
        spdy::fuzzing::{
            connection as spdy_connection, frames as spdy_frames, header_block as spdy_header_block,
        },
    };
}

const ADDR: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// How long a client may take to send the head of its request, which only
/// applies with a timer. A connection which never sends one would stay open
/// otherwise. Clients send it right after they connected, and every
/// connection serves one request, see [`MAX_CONNECTIONS`]. Far below the 30
/// seconds of hyper, so that a connection without a request holds its
/// permit for a short time only. That does not bound how long a local
/// process can hold the server off: one which keeps opening connections,
/// holds all permits and keeps the backlog of the listener full makes the
/// connection of another client wait behind its own ones, a few times
/// [`UPGRADE_TIMEOUT`], or get its handshake dropped and retried while the
/// backlog is full, for as long as it keeps that up. It cannot use up the
/// file descriptors of the server.
const HEADER_READ_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(5)
};

/// How long a connection may take until it got upgraded, also one which
/// does not read its response. Clients make one upgrade request per
/// connection, hyper has no timeout for writing the response.
const UPGRADE_TIMEOUT: Duration = HEADER_READ_TIMEOUT.saturating_mul(2);

/// The most connections which did not get upgraded yet. More wait to be
/// accepted, so that local processes which open connections without a
/// request cannot use up the descriptors the whole pod needs. Upgraded
/// connections do not count, a separate active session budget bounds them.
/// There is no limit per peer, the server listens on the loopback address
/// only, where the kubelet and a local process which holds connections look
/// the same.
const MAX_CONNECTIONS: usize = if cfg!(test) { 8 } else { 256 };

/// Default limit shared by active exec, attach and port forward sessions.
pub(crate) const DEFAULT_MAX_ACTIVE_SESSIONS: u16 = 256;

/// The most a connection buffers of the head of its request, the request
/// line and the headers, see [`MAX_CONNECTIONS`]. A longer one gets a 431
/// response. hyper takes about 400 KiB otherwise, which lets local processes
/// make the server hold about 100 MiB without a token. The kubelet sends
/// the headers of the client and its own, a few KiB, Go servers take up to
/// 1 MiB by default, nginx 32 KiB.
const MAX_HEADER_BUF_SIZE: usize = 64 * 1024;

const EXEC_PATH: &str = "exec";
const ATTACH_PATH: &str = "attach";
const PORT_FORWARD_PATH: &str = "portforward";

/// The largest WebSocket message and frame a client of exec or attach may
/// send. Clients send stdin, which kubectl reads in 32 KiB chunks and the
/// attach sockets take in packets of up to 33 KiB, and small resize events. The
/// Python client sends all stdin of a write in one message, 1 MiB at a time
/// for `k8s_cp` of Ansible, a third more in base64. The defaults of 64 MiB
/// per message, or the 32 MiB of Kubernetes, would let a client make the
/// server buffer that much per message. A client which sends more fails its
/// session with a status and a close which tell why.
const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

/// The largest WebSocket message and frame a client of port forwarding may
/// send. A single message must not carry more than a connection takes while
/// it gets established, the `CONNECT_BUFFER` of [`port_forward`], nor all
/// that the ports of a WebSocket hold together, its `CHANNEL_BUFFER`. A
/// client which sends more gets the WebSocket closed with a code which tells
/// why.
const PORT_FORWARD_MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// The error of a request for a URL while too many of its kind wait for
/// their client. Clients match it, see `ErrTooManyRequests` of the Go client,
/// so it has to stay the start of the message.
const TOO_MANY_REQUESTS: &str = "maximum number of in-flight requests exceeded";

/// How often the sessions whose client did not come in time get dropped,
/// while sessions wait.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Serves the exec, attach and port forward sessions over HTTP on the
/// loopback address, each under a one time URL, see
/// [`StreamingServer::exec_url`].
#[derive(Debug)]
pub(crate) struct StreamingServer {
    /// The port the server listens on, once it got started for the first
    /// session.
    port: OnceCell<u16>,
    state: Arc<Mutex<State>>,
}

/// State handled by the streaming server, the sessions waiting for their
/// client by token. Never locked across an await.
#[derive(Debug)]
struct State {
    sessions: BoundedMap<RandomId, Session>,
    port_forwards: BoundedMap<RandomId, PortForward>,

    /// Held from a valid upgrade request until its session finishes,
    /// including the wait for the HTTP connection to upgrade.
    active: Arc<Semaphore>,
    max_active_sessions: usize,
    resources: Arc<Resources>,

    /// Whether a task drops the sessions whose client did not come in time,
    /// see [`StreamingServer::sweep`].
    sweeping: bool,
}

/// A dedicated session for each provided functionality. Not `Debug`, the
/// command of an exec may carry secrets.
enum Session {
    Exec(Box<ExecSession>),
    Attach(AttachSession),
}

/// What the exec and attach sessions have in common.
#[derive(Debug)]
pub(crate) struct SessionCommon {
    pub(crate) container_id: Box<str>,
    pub(crate) stdin: bool,
    pub(crate) stdout: bool,
    pub(crate) stderr: bool,
    pub(crate) idle_timeout: Option<Duration>,
}

#[cfg(test)]
impl Default for StreamingServer {
    fn default() -> Self {
        Self::new(usize::from(DEFAULT_MAX_ACTIVE_SESSIONS))
    }
}

impl StreamingServer {
    /// A server with a shared budget for all active streaming sessions.
    #[cfg(test)]
    pub(crate) fn new(max_active_sessions: usize) -> Self {
        Self::with_limits(
            max_active_sessions,
            u32::from(DEFAULT_BUFFER_MIB) * 1024 * 1024,
            usize::from(DEFAULT_FORWARD_CONNECTIONS),
        )
    }

    pub(crate) fn with_limits(
        max_active_sessions: usize,
        buffer_bytes: u32,
        connections: usize,
    ) -> Self {
        Self {
            port: OnceCell::new(),
            state: Arc::new(Mutex::new(State {
                sessions: BoundedMap::default(),
                port_forwards: BoundedMap::default(),
                active: Arc::new(Semaphore::new(max_active_sessions)),
                max_active_sessions,
                resources: Resources::new(buffer_bytes, connections),
                sweeping: false,
            })),
        }
    }

    /// Start the streaming server if not already running and return its
    /// port.
    async fn start_if_required(&self) -> Result<u16> {
        let port = self.port.get_or_try_init(|| async {
            let listener = TcpListener::bind((ADDR, 0))
                .await
                .context("bind streaming server")?;

            let local_addr = listener
                .local_addr()
                .context("get listeners local address")?;

            info!("Starting streaming server on {local_addr}");
            task::spawn_local(Self::serve(listener, self.state.clone()));
            Ok::<_, anyhow::Error>(local_addr.port())
        });
        port.await.copied().context("start streaming server")
    }

    /// The port of the started server.
    #[cfg(test)]
    fn port(&self) -> u16 {
        *self.port.get().expect("streaming server started")
    }

    /// Serve the main streaming server, every connection in a task of its
    /// own, at most [`MAX_CONNECTIONS`] at once, each for one request.
    async fn serve(listener: TcpListener, state: Arc<Mutex<State>>) {
        let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let mut accept_errors = listener::AcceptErrors::default();
        loop {
            let Ok(permit) = connections.clone().acquire_owned().await else {
                return;
            };
            let stream = match Self::accept(&listener).await {
                Ok(stream) => stream,
                Err(e) => {
                    accept_errors.failed(&e).await;
                    continue;
                }
            };
            let state = state.clone();
            task::spawn(async move {
                // Until the connection ended or got upgraded.
                let _permit = permit;
                let service = service_fn(move |request| {
                    let response = Self::route(request, &state);
                    async move { Ok::<_, Infallible>(response) }
                });
                let connection = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_READ_TIMEOUT)
                    .max_buf_size(MAX_HEADER_BUF_SIZE)
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades();
                match time::timeout(UPGRADE_TIMEOUT, connection).await {
                    Ok(Err(e)) => trace!("Unable to serve connection: {e}"),
                    Ok(Ok(())) => {}
                    Err(_) => debug!("Closing connection which did not end in time"),
                }
            });
        }
    }

    /// Accept the next connection. Like the Go server of CRI-O without
    /// Nagle's algorithm, so that small writes in a row, like the last output
    /// and the status of a session, do not wait for a delayed ACK.
    async fn accept(listener: &TcpListener) -> io::Result<TcpStream> {
        let (stream, _) = listener.accept().await?;
        if let Err(e) = stream.set_nodelay(true) {
            debug!("Unable to disable Nagle's algorithm: {e}");
        }
        Ok(stream)
    }

    /// Drop the sessions whose client did not come in time, otherwise they
    /// would stay until the next request. The task ends once no session
    /// waits or the server is gone, so that an idle server does not wake up.
    async fn sweep(state: Weak<Mutex<State>>) {
        loop {
            time::sleep(SWEEP_INTERVAL).await;
            let Some(state) = state.upgrade() else {
                return;
            };
            let mut state = lock(&state);
            state.sessions.sweep();
            state.port_forwards.sweep();
            if state.sessions.is_empty() && state.port_forwards.is_empty() {
                state.sweeping = false;
                return;
            }
        }
    }

    /// Route a request and log it with the status of its response.
    fn route(request: Request<Incoming>, state: &Mutex<State>) -> Response {
        let logged = tracing::enabled!(Level::DEBUG).then(|| {
            (
                request.method().clone(),
                Self::logged_path(request.uri().path()),
            )
        });
        let mut response = Self::dispatch(request, state);
        if let Some((method, path)) = logged {
            debug!("{method} {path}: {}", response.status());
        }
        // One request per connection, clients make one upgrade request each.
        // A client which keeps sending requests without reading the
        // responses would hold its connection otherwise, see
        // [`MAX_CONNECTIONS`]. Turning keep-alive off for the connection
        // would add the close to the upgrades as well.
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            response
                .headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
        }
        response
    }

    /// The path of a request for the logs, which shows only the start of its
    /// token, see [`RandomId`].
    fn logged_path(path: &str) -> String {
        match path.strip_prefix('/').and_then(|path| path.split_once('/')) {
            Some((kind, token)) => {
                let start: String = token.chars().take(8).collect();
                format!("/{:?}/{:?}...", ClientStr(kind), ClientStr(&start))
            }
            None => format!("{:?}", ClientStr(path)),
        }
    }

    /// Hand a request to the handler of its path, `/<kind>/<token>`. Like a
    /// GET, a HEAD gets handled, the response has no body then.
    fn dispatch(request: Request<Incoming>, state: &Mutex<State>) -> Response {
        let Some((kind, token)) = request
            .uri()
            .path()
            .strip_prefix('/')
            .and_then(|path| path.split_once('/'))
        else {
            return web::status(StatusCode::NOT_FOUND);
        };
        // The path of the kind, which outlives the request.
        let Some(kind) = [EXEC_PATH, ATTACH_PATH, PORT_FORWARD_PATH]
            .into_iter()
            .find(|path| *path == kind)
        else {
            return web::status(StatusCode::NOT_FOUND);
        };
        if token.is_empty() || token.contains('/') {
            return web::status(StatusCode::NOT_FOUND);
        }
        if !matches!(*request.method(), Method::GET | Method::HEAD | Method::POST) {
            let mut response = web::status(StatusCode::METHOD_NOT_ALLOWED);
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET,HEAD,POST"));
            return response;
        }
        let token = match token.parse() {
            Ok(token) => token,
            Err(e) => return web::text(StatusCode::BAD_REQUEST, format!("Invalid URL: {e}")),
        };
        if kind == PORT_FORWARD_PATH {
            Self::handle_port_forward(token, state, request)
        } else {
            Self::handle(kind, token, state, request)
        }
    }

    /// Start the server if required and return the URL of the exec session.
    pub(crate) async fn exec_url(&self, session: ExecSession) -> Result<String> {
        self.wait_for_client(
            EXEC_PATH,
            |state| &mut state.sessions,
            Session::Exec(session.into()),
        )
        .await
    }

    /// Start the server if required and return the URL of the attach
    /// session.
    pub(crate) async fn attach_url(&self, session: AttachSession) -> Result<String> {
        self.wait_for_client(
            ATTACH_PATH,
            |state| &mut state.sessions,
            Session::Attach(session),
        )
        .await
    }

    /// Start the server if required and return the URL for forwarding ports
    /// into the network namespace.
    pub(crate) async fn port_forward_url(
        &self,
        netns_path: PathBuf,
        idle_timeout: Option<Duration>,
    ) -> Result<String> {
        let resources = lock(&self.state).resources.clone();
        self.wait_for_client(
            PORT_FORWARD_PATH,
            |state| &mut state.port_forwards,
            PortForward::with_resources(netns_path, idle_timeout, resources),
        )
        .await
    }

    /// Start the server if required and keep a session in its map until its
    /// client takes it, return its URL under `path`. Too many sessions which
    /// wait for their client fail a new one, like for the kubelet, instead
    /// of dropping one whose client would come.
    async fn wait_for_client<V>(
        &self,
        path: &str,
        sessions: impl FnOnce(&mut State) -> &mut BoundedMap<RandomId, V>,
        session: V,
    ) -> Result<String> {
        let port = self.start_if_required().await?;
        let token = RandomId::new();
        let mut state = lock(&self.state);
        if sessions(&mut state).insert(token, session).is_err() {
            bail!(TOO_MANY_REQUESTS);
        }
        if !state.sweeping {
            state.sweeping = true;
            task::spawn(Self::sweep(Arc::downgrade(&self.state)));
        }
        Ok(format!("http://{ADDR}:{port}/{path}/{token}"))
    }

    /// Take the exec or attach session of a valid request. Requests which get
    /// rejected before leave the token, so that the client can retry.
    fn take_session(
        state: &Mutex<State>,
        token: RandomId,
    ) -> Result<(Session, OwnedSemaphorePermit), StatusCode> {
        let mut state = lock(state);
        if !state.sessions.contains_key(&token) {
            debug!("Session got taken by another request or expired");
            return Err(StatusCode::NOT_FOUND);
        }
        let permit = Self::active_permit(&state)?;
        let session = state.sessions.remove(&token).ok_or(StatusCode::NOT_FOUND)?;
        // Only what identifies the session, the command of an exec may carry
        // secrets.
        info!(
            "Got valid {} session for container {} and token {token:?}",
            session.typ(),
            session.common().container_id
        );
        Ok((session, permit))
    }

    /// Take the session of a valid request. Requests which get rejected
    /// before leave the token, so that the client can retry. Another request
    /// may have taken it meanwhile.
    fn take_port_forward(
        state: &Mutex<State>,
        token: RandomId,
    ) -> Result<(PortForward, OwnedSemaphorePermit), StatusCode> {
        let mut state = lock(state);
        if !state.port_forwards.contains_key(&token) {
            debug!("Port forward session got taken by another request or expired");
            return Err(StatusCode::NOT_FOUND);
        }
        let permit = Self::active_permit(&state)?;
        let session = state
            .port_forwards
            .remove(&token)
            .ok_or(StatusCode::NOT_FOUND)?;
        Ok((session, permit))
    }

    /// Reject excess sessions before consuming their token, allowing retry
    /// once another session ends.
    fn active_permit(state: &State) -> Result<OwnedSemaphorePermit, StatusCode> {
        state.active.clone().try_acquire_owned().map_err(|_| {
            debug!(
                limit = state.max_active_sessions,
                "Active streaming session limit reached"
            );
            StatusCode::TOO_MANY_REQUESTS
        })
    }
}

impl Session {
    /// The kind of the session for the logs, the path of its URL.
    fn typ(&self) -> &'static str {
        match self {
            Session::Exec(_) => EXEC_PATH,
            Session::Attach(_) => ATTACH_PATH,
        }
    }

    /// What both kinds of sessions have.
    fn common(&self) -> &SessionCommon {
        match self {
            Session::Exec(s) => &s.common,
            Session::Attach(s) => &s.common,
        }
    }

    /// Whether the process of the session has a terminal.
    fn terminal(&self) -> bool {
        match self {
            Session::Exec(s) => s.tty,
            Session::Attach(s) => s.child.io().attach().is_terminal(),
        }
    }
}

#[cfg(test)]
mod tests;
