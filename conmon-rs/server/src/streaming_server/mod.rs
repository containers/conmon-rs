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
use std::{
    convert::Infallible,
    fmt::Debug,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
};
use stream_protocol::ClientStr;
use tokio::{
    net::TcpListener,
    sync::{OnceCell, Semaphore},
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
mod session;
mod spdy;
mod stream_protocol;
#[cfg(test)]
mod testing;
mod web;

pub use attach::AttachSession;
#[cfg(test)]
pub(crate) use bounded_map::DEFAULT_CAPACITY;
pub use exec::ExecSession;

/// The entry points of the fuzz targets.
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzzing {
    pub use super::{
        channel::fuzzing::channel,
        handshake::fuzzing::upgrade_headers,
        port_forward::fuzzing::{query_ports, spdy as port_forward_spdy},
        remotecommand::fuzzing::{resize_events, spdy as remotecommand_spdy},
        session::fuzzing::{client_messages, pending_stdin},
        spdy::fuzzing::{
            connection as spdy_connection, frames as spdy_frames, header_block as spdy_header_block,
        },
    };
}

const ADDR: &str = "127.0.0.1";

/// How long a client may take to send the head of its request, which only
/// applies with a timer. A connection which never sends one would stay open
/// otherwise. Clients send it right after they connected, and every
/// connection serves one request, see [`MAX_CONNECTIONS`]. Far below the 30
/// seconds of hyper, so that connections without a request cannot hold the
/// server off for long: a local process which keeps all of them open delays
/// the requests of others by up to this long, the backlog of the listener
/// serves them in order.
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
/// connections do not count, their tokens bound them. There is no limit per
/// peer, the server listens on the loopback address only, where the kubelet
/// and a local process which holds connections look the same.
const MAX_CONNECTIONS: usize = if cfg!(test) { 8 } else { 256 };

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
/// attach sockets cap at 64 KiB per packet, and small resize events. The
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

#[derive(Debug, Default)]
/// The main streaming server structure of this module.
pub struct StreamingServer {
    /// The port the server listens on, once it got started for the first
    /// session.
    port: OnceCell<u16>,
    state: Arc<Mutex<State>>,
}

/// State handled by the streaming server, the sessions waiting for their
/// client by token. Never locked across an await.
#[derive(Debug, Default)]
struct State {
    sessions: BoundedMap<RandomId, Session>,
    port_forwards: BoundedMap<RandomId, PortForward>,

    /// Whether a task drops the sessions whose client did not come in time,
    /// see [`StreamingServer::sweep`].
    sweeping: bool,
}

#[derive(Debug)]
/// A dedicated session for each provided functionality.
enum Session {
    Exec(Box<ExecSession>),
    Attach(AttachSession),
}

impl StreamingServer {
    /// Start the streaming server if not already running and return its
    /// port.
    async fn start_if_required(&self) -> Result<u16> {
        let port = self.port.get_or_try_init(|| async {
            let listener = TcpListener::bind(ADDR.to_string() + ":0")
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
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    accept_errors.failed(&e).await;
                    continue;
                }
            };
            let state = state.clone();
            tokio::spawn(async move {
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
    pub async fn exec_url(&self, session: ExecSession) -> Result<String> {
        self.wait_for_client(
            EXEC_PATH,
            |state| &mut state.sessions,
            Session::Exec(session.into()),
        )
        .await
    }

    /// Start the server if required and return the URL of the attach
    /// session.
    pub async fn attach_url(&self, session: AttachSession) -> Result<String> {
        self.wait_for_client(
            ATTACH_PATH,
            |state| &mut state.sessions,
            Session::Attach(session),
        )
        .await
    }

    /// Start the server if required and return the URL for forwarding ports
    /// into the network namespace.
    pub async fn port_forward_url(
        &self,
        netns_path: PathBuf,
        idle_timeout: Option<Duration>,
    ) -> Result<String> {
        self.wait_for_client(
            PORT_FORWARD_PATH,
            |state| &mut state.port_forwards,
            PortForward::new(netns_path, idle_timeout),
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
    ) -> Result<String>
    where
        V: Debug,
    {
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
    fn take_session(state: &Mutex<State>, token: RandomId) -> Option<Session> {
        let Some(session) = lock(state).sessions.remove(&token) else {
            debug!("Session got taken by another request or expired");
            return None;
        };
        // Only what identifies the session, the command of an exec may carry
        // secrets.
        info!(
            "Got valid {} session for container {} and token {token:?}",
            session.typ(),
            session.container_id()
        );
        Some(session)
    }

    /// Take the session of a valid request. Requests which get rejected
    /// before leave the token, so that the client can retry. Another request
    /// may have taken it meanwhile.
    fn take_port_forward(state: &Mutex<State>, token: RandomId) -> Option<PortForward> {
        let session = lock(state).port_forwards.remove(&token);
        if session.is_none() {
            debug!("Port forward session got taken by another request or expired");
        }
        session
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

    fn idle_timeout(&self) -> Option<Duration> {
        match self {
            Session::Exec(s) => s.idle_timeout,
            Session::Attach(s) => s.idle_timeout,
        }
    }

    /// Whether the process of the session has a terminal.
    fn terminal(&self) -> bool {
        match self {
            Session::Exec(s) => s.tty,
            Session::Attach(s) => s.child.io().terminal_eof().is_some(),
        }
    }
}

#[cfg(test)]
mod tests;
