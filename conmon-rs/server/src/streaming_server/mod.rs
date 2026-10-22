//! The streaming server of exec, attach and port forward sessions: the URLs
//! with their one time tokens and the HTTP server their clients connect to.

use crate::{
    bounded_hashmap::BoundedHashMap,
    listener,
    port_forward::PortForward,
    random_id::RandomId,
    stream_protocol::ClientStr,
    web::{self, Response},
};
use anyhow::{Context, Result, bail};
use hyper::{
    Method, Request, StatusCode,
    body::Incoming,
    header::{self, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    fmt::Debug,
    path::PathBuf,
    sync::{Arc, Weak},
};
use tokio::{
    net::TcpListener,
    sync::RwLock,
    task,
    time::{self, Duration},
};
use tracing::{Level, debug, info, trace};

mod attach;
mod exec;
mod handshake;
mod session;
#[cfg(test)]
mod testing;

pub use attach::AttachSession;
pub use exec::ExecSession;
#[cfg(feature = "fuzzing")]
pub use handshake::fuzzing;

const ADDR: &str = "127.0.0.1";

/// How long a client may take to send the head of its request, the default
/// of hyper, which only applies with a timer. A connection which never sends
/// one would stay open otherwise.
const HEADER_READ_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(30)
};

const EXEC_PATH: &str = "exec";
const ATTACH_PATH: &str = "attach";
const PORT_FORWARD_PATH: &str = "portforward";

/// The largest WebSocket message and frame a client may send. Clients send
/// stdin, which kubectl reads in 32 KiB chunks and the attach sockets cap at
/// 64 KiB per packet, and small resize events. The defaults of 64 MiB per
/// message would let a client make the server buffer that much per message.
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// How often the sessions whose client did not come in time get dropped,
/// while sessions wait.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

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
    sessions: BoundedHashMap<RandomId, Session>,
    port_forwards: BoundedHashMap<RandomId, PortForward>,

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

    /// Serve the main streaming server, every connection in a task of its
    /// own.
    async fn serve(listener: TcpListener, state: Arc<RwLock<State>>) {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    listener::accept_failed(&e).await;
                    continue;
                }
            };
            let state = state.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let state = state.clone();
                    async move { Ok::<_, Infallible>(Self::route(request, state).await) }
                });
                if let Err(e) = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_READ_TIMEOUT)
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await
                {
                    trace!("Unable to serve connection: {e}");
                }
            });
        }
    }

    /// Drop the sessions whose client did not come in time, otherwise they
    /// would stay until the next request. The task ends once no session
    /// waits or the server is gone, so that an idle server does not wake up.
    async fn sweep(state: Weak<RwLock<State>>) {
        loop {
            time::sleep(SWEEP_INTERVAL).await;
            let Some(state) = state.upgrade() else {
                return;
            };
            let mut state = state.write().await;
            state.sessions.sweep();
            state.port_forwards.sweep();
            if state.sessions.is_empty() && state.port_forwards.is_empty() {
                state.sweeping = false;
                return;
            }
        }
    }

    /// Return the URL for a specific path and token.
    fn url_for(&self, p: &str, token: &RandomId) -> String {
        format!("http://{ADDR}:{0}/{p}/{token}", self.port)
    }

    /// Route a request and log it with the status of its response.
    async fn route(request: Request<Incoming>, state: Arc<RwLock<State>>) -> Response {
        let logged = tracing::enabled!(Level::DEBUG).then(|| {
            (
                request.method().clone(),
                Self::logged_path(request.uri().path()),
            )
        });
        let response = Self::dispatch(request, state).await;
        if let Some((method, path)) = logged {
            debug!("{method} {path}: {}", response.status());
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
    async fn dispatch(request: Request<Incoming>, state: Arc<RwLock<State>>) -> Response {
        let Some((kind, token)) = request
            .uri()
            .path()
            .strip_prefix('/')
            .and_then(|path| path.split_once('/'))
        else {
            return web::status(StatusCode::NOT_FOUND);
        };
        let port_forward = match kind {
            EXEC_PATH | ATTACH_PATH => false,
            PORT_FORWARD_PATH => true,
            _ => return web::status(StatusCode::NOT_FOUND),
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
        if port_forward {
            Self::handle_port_forward(token, state, request).await
        } else {
            Self::handle(token, state, request).await
        }
    }

    /// Returns the URL used for the provided exec session.
    pub async fn exec_url(&self, session: ExecSession) -> Result<String> {
        let token = self
            .wait_for_client(|state| &mut state.sessions, Session::Exec(session.into()))
            .await?;
        Ok(self.url_for(EXEC_PATH, &token))
    }

    /// Returns the URL used for the provided attach session.
    pub async fn attach_url(&self, session: AttachSession) -> Result<String> {
        let token = self
            .wait_for_client(|state| &mut state.sessions, Session::Attach(session))
            .await?;
        Ok(self.url_for(ATTACH_PATH, &token))
    }

    /// Returns the URL used for forwarding ports into the network namespace.
    pub async fn port_forward_url(
        &self,
        netns_path: PathBuf,
        idle_timeout: Option<Duration>,
    ) -> Result<String> {
        let token = self
            .wait_for_client(
                |state| &mut state.port_forwards,
                PortForward::new(netns_path, idle_timeout),
            )
            .await?;
        Ok(self.url_for(PORT_FORWARD_PATH, &token))
    }

    /// Keep a session in its map until its client takes it, return its
    /// token. Too many sessions which wait for their client fail a new one,
    /// like for the kubelet, instead of dropping one whose client would come.
    async fn wait_for_client<V>(
        &self,
        sessions: impl FnOnce(&mut State) -> &mut BoundedHashMap<RandomId, V>,
        session: V,
    ) -> Result<RandomId>
    where
        V: Debug,
    {
        let token = RandomId::new();
        let mut state = self.state.write().await;
        if sessions(&mut state).insert(token, session).is_err() {
            bail!("maximum number of in-flight requests exceeded");
        }
        if !state.sweeping {
            state.sweeping = true;
            task::spawn(Self::sweep(Arc::downgrade(&self.state)));
        }
        Ok(token)
    }

    /// Take the exec or attach session of a valid request. Requests which get
    /// rejected before leave the token, so that the client can retry.
    async fn take_session(state: &RwLock<State>, token: RandomId) -> Option<Session> {
        let Some(session) = state.write().await.sessions.remove(&token) else {
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
    async fn take_port_forward(state: &RwLock<State>, token: RandomId) -> Option<PortForward> {
        let session = state.write().await.port_forwards.remove(&token);
        if session.is_none() {
            debug!("Port forward session got taken by another request or expired");
        }
        session
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

    fn idle_timeout(&self) -> Option<Duration> {
        match self {
            Session::Exec(s) => s.idle_timeout,
            Session::Attach(s) => s.idle_timeout,
        }
    }

    /// Whether the process of the session has a terminal.
    async fn terminal(&self) -> bool {
        match self {
            Session::Exec(s) => s.tty,
            Session::Attach(s) => s.child.io().terminal_eof().await.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::{
        bounded_hashmap::DEFAULT_CAPACITY,
        random_id::RandomId,
        streaming_server::{ADDR, HEADER_READ_TIMEOUT, SWEEP_INTERVAL, StreamingServer},
    };
    use anyhow::{Context, Result};

    use std::path::PathBuf;
    use tokio::{
        io::AsyncReadExt,
        net::TcpStream,
        task::LocalSet,
        time::{self, Duration},
    };

    #[tokio::test]
    async fn connections_without_a_request_get_closed() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                let mut stream = TcpStream::connect((ADDR, server.port)).await?;
                let mut rest = vec![];
                time::timeout(4 * HEADER_READ_TIMEOUT, stream.read_to_end(&mut rest))
                    .await
                    .context("connection still open")??;
                Ok(())
            })
            .await
    }

    #[tokio::test(start_paused = true)]
    async fn sessions_wait_for_their_client_in_bounds() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                // Nothing wakes up the server while no session waits.
                assert!(!server.state.read().await.sweeping);
                let mut urls = vec![];
                for _ in 0..DEFAULT_CAPACITY {
                    urls.push(server.port_forward_url(PathBuf::new(), None).await?);
                }
                // A full map fails new sessions instead of dropping one whose
                // client would come.
                let err = server
                    .port_forward_url(PathBuf::new(), None)
                    .await
                    .expect_err("full");
                assert!(err.to_string().contains("in-flight"), "{err:#}");
                assert_eq!(
                    server.state.read().await.port_forwards.len(),
                    DEFAULT_CAPACITY
                );

                // The sessions whose client did not come go without a request,
                // then the sweeps stop until the next session.
                time::sleep(Duration::from_secs(60) + 2 * SWEEP_INTERVAL).await;
                assert_eq!(server.state.read().await.port_forwards.len(), 0);
                assert!(!server.state.read().await.sweeping);
                server.port_forward_url(PathBuf::new(), None).await?;
                assert!(server.state.read().await.sweeping);
                Ok(())
            })
            .await
    }

    #[test]
    fn logged_paths_hide_the_tokens() {
        let token = RandomId::new().to_string();
        let logged = StreamingServer::logged_path(&format!("/exec/{token}"));
        assert_eq!(logged, format!("/\"exec\"/{:?}...", &token[..8]));
        assert!(!logged.contains(&token[..9]));
        assert_eq!(StreamingServer::logged_path("/\n"), r#""/\n""#);
    }
}
