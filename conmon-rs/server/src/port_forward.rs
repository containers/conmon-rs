//! Port forwarding into the network namespace of a pod, the Kubernetes
//! `portforward.k8s.io` protocol, as k8s.io/cri-streaming serves it for
//! CRI-O otherwise.
//!
//! Over SPDY, the client opens two streams per forwarded connection, both
//! with the `port` and `requestID` headers: an `error` stream, on which the
//! server reports why forwarding failed, and a `data` stream, which carries
//! the connection. The kubelet always uses SPDY, direct clients like crictl
//! may tunnel it through a WebSocket instead.
//!
//! A request waits for its second stream without reading the first one.
//! client-go opens the error stream first and waits for its reply, a client
//! which sends a lot on a data stream before opening its error stream holds
//! up the connection until the request times out.
//!
//! The older WebSocket protocol forwards one connection per port in the
//! query of the request. Each port gets a data and an error channel, every
//! message starts with its channel and the first one of each channel carries
//! the port.

use crate::{
    budget::{Budget, Held, HeldBuf},
    channel,
    idle::Activity,
    spdy::{Connection, Headers, PING_INTERVAL, Stream, StreamReceiver, StreamSender},
    stream_protocol::{ClientStr, STREAM_CREATION_TIMEOUT, STREAM_TYPE_HEADER},
    web::{self, Message, WebSocket},
};
use anyhow::{Context, Result, bail, ensure};
use bytes::{BufMut, Bytes, BytesMut};
use futures::{SinkExt, StreamExt, stream::FuturesUnordered};
use nix::{
    sched::{CloneFlags, setns},
    sys::socket::{AddressFamily, SockFlag, SockType, socket},
};
use percent_encoding::percent_decode_str;
use std::{
    collections::HashMap,
    fs::{self, File},
    future::{self, Future},
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream as StdTcpStream},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    pin::{Pin, pin},
    sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc as std_mpsc},
    thread,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream},
    net::{TcpSocket, TcpStream, tcp::OwnedWriteHalf},
    select,
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::{self, Duration},
};
use tokio_tungstenite::WebSocketStream;
use tokio_util::{
    sync::{CancellationToken, DropGuard},
    time::{DelayQueue, delay_queue::Key},
};
use tracing::{debug, trace};

/// The protocol the client has to ask for in the upgrade request.
pub const PROTOCOL: &str = "portforward.k8s.io";

const PORT_HEADER: &str = "port";
const REQUEST_ID_HEADER: &str = "requestID";
const STREAM_TYPE_DATA: &str = "data";
const STREAM_TYPE_ERROR: &str = "error";

/// How long connecting to a port in the pod may take. Within the pod it gets
/// accepted or refused right away, unless a firewall drops it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The stack of the thread which creates the sockets of a session within its
/// network namespace.
const SOCKET_THREAD_STACK_SIZE: usize = 128 * 1024;

/// How long the thread waits for the next connection before it ends, which
/// leaves the network namespace.
const SOCKET_THREAD_LINGER: Duration = Duration::from_secs(1);

/// How much a client may send for a connection before it got established in
/// the pod, a client which sends more fails the forwarding.
const CONNECT_BUFFER: usize = 1024 * 1024;

/// How much the connections of a session which get established hold for the
/// client together. Their data moves from the byte budget of the client into
/// this one, so that connections to a port which does not answer, which the
/// connect timeout could take long, do not stop the others. Once it is used
/// up, connecting streams stop taking their data, which then stays in the
/// byte budget of the client and may stop reading the client like any
/// stream which does not take its data. Any number of connections get
/// established at once, like with CRI-O.
const CONNECT_BUDGET: u32 = 16 * 1024 * 1024;

/// How much of the container output goes into a single frame, as much as Go
/// clients send per frame.
const READ_BUFFER: usize = 32 * 1024;

/// How long the client gets to close its side of a connection after the
/// container closed its side, like CRI-O waits.
const CLOSE_GRACE_PERIOD: Duration = Duration::from_secs(1);

/// The WebSocket subprotocol of SPDY tunneled through a WebSocket.
const SPDY_TUNNEL_PROTOCOL: &str = "SPDY/3.1+portforward.k8s.io";

/// The WebSocket subprotocols of the channel protocol, which is binary
/// without a subprotocol as well.
const CHANNEL_PROTOCOL: &str = "v4.channel.k8s.io";
const BASE64_CHANNEL_PROTOCOL: &str = "v4.base64.channel.k8s.io";

/// The query parameter with the ports of the channel protocol.
const PORT_QUERY: &str = "port";

/// The most ports of a channel protocol request. Base64 clients get the
/// channel as an ASCII digit offset, which stays a single byte for the
/// channels of these ports.
const MAX_CHANNEL_PORTS: usize = 40;

/// How many messages of the client wait for a forwarded connection, before
/// the WebSocket is not read anymore.
const CHANNEL_QUEUE: usize = 16;

/// How many bytes of the client the ports of a WebSocket hold together,
/// before the WebSocket is not read anymore. Without it, a client could make
/// the server hold [`CHANNEL_QUEUE`] messages of up to the largest message
/// size for each of [`MAX_CHANNEL_PORTS`] ports which do not take them.
const CHANNEL_BUFFER: u32 = 4 * 1024 * 1024;

/// How many messages for the client wait for the WebSocket.
const OUTPUT_QUEUE: usize = 16;

/// The size of the buffers between the WebSocket and the tunneled SPDY
/// connection.
const TUNNEL_BUFFER: usize = 64 * 1024;

/// A protocol a client may forward ports with over a WebSocket.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WebSocketProtocol {
    /// SPDY tunneled through binary messages.
    SpdyTunnel,

    /// The channel protocol, its payloads base64 encoded in text messages or
    /// as they are in binary ones.
    Channels { base64: bool },
}

impl WebSocketProtocol {
    /// The protocol for the subprotocols the client requested, with the one
    /// the server answers with. The tunnel wins like for the kubelet,
    /// otherwise the first supported one, none means binary channels.
    pub fn negotiate<'a>(
        requested: impl IntoIterator<Item = &'a str>,
    ) -> Option<(Self, Option<&'static str>)> {
        let requested: Vec<_> = requested.into_iter().collect();
        if requested.contains(&SPDY_TUNNEL_PROTOCOL) {
            return Some((Self::SpdyTunnel, Some(SPDY_TUNNEL_PROTOCOL)));
        }
        if requested.is_empty() {
            return Some((Self::Channels { base64: false }, None));
        }
        requested.into_iter().find_map(|protocol| match protocol {
            CHANNEL_PROTOCOL => Some((Self::Channels { base64: false }, Some(CHANNEL_PROTOCOL))),
            BASE64_CHANNEL_PROTOCOL => Some((
                Self::Channels { base64: true },
                Some(BASE64_CHANNEL_PROTOCOL),
            )),
            _ => None,
        })
    }
}

/// The data a client sends for a forwarded connection. It counts against
/// the budget of the client until it got written to the pod.
trait Input {
    /// The next data, `None` once the client is done.
    async fn recv(&mut self) -> Option<Held>;
}

impl Input for StreamReceiver {
    async fn recv(&mut self) -> Option<Held> {
        self.recv_held().await
    }
}

impl Input for mpsc::Receiver<Held> {
    async fn recv(&mut self) -> Option<Held> {
        mpsc::Receiver::recv(self).await
    }
}

/// What a client sent while its connection got established, within the
/// connect budget of the session, see [`CONNECT_BUDGET`]. It gets collected
/// into one buffer, small data does not take an allocation each.
#[derive(Debug, Default)]
struct Early {
    data: HeldBuf,

    /// What the client sent last, which did not get into the connect budget
    /// before the connection got established.
    last: Option<Held>,

    /// Whether the client was done already.
    ended: bool,
}

/// Sends the messages of a port to the client.
#[derive(Clone)]
struct ChannelOutput {
    messages: mpsc::Sender<Message>,
    base64: bool,
    data: u8,
    error: u8,
}

impl ChannelOutput {
    /// Send data on a channel, fails once the client went away.
    async fn send(&self, channel: u8, data: &[u8]) -> Result<()> {
        let message = channel::message(self.base64, channel, data);
        self.messages
            .send(message)
            .await
            .map_err(|_| anyhow::anyhow!("WebSocket closed"))
    }
}

/// A port forward session of a pod.
#[derive(Debug)]
pub struct PortForward {
    netns_path: PathBuf,
    idle_timeout: Option<Duration>,
    ping_interval: Option<Duration>,
}

/// The streams of a request, until both arrived.
struct PendingRequest {
    data: Option<Waiting>,
    error: Option<Waiting>,

    /// The deadline of the request in [`Pending::deadlines`].
    deadline: Key,
}

/// A stream which waits for the other one of its request. Its reset gets
/// watched until it stops waiting, see [`Pending::resets`].
struct Waiting {
    stream: Stream,
    _watch: DropGuard,
}

impl Waiting {
    fn is_reset(&self) -> bool {
        self.stream.is_reset()
    }
}

/// The watch of a waiting stream, which names its request once the client
/// reset it, nothing once the stream stopped waiting.
type ResetWatch = Pin<Box<dyn Future<Output = Option<Arc<str>>> + Send>>;

/// The requests waiting for their second stream, with their deadlines and
/// the watches of their streams. All are bounded by the streams of a
/// connection.
struct Pending {
    requests: HashMap<Arc<str>, PendingRequest>,
    deadlines: DelayQueue<Arc<str>>,

    /// The watches of the waiting streams, which let go of a stream once
    /// the client reset it. Otherwise the streams of a client which opens
    /// and resets them would count against the most streams of its
    /// connection until their deadline, as no new stream which could free
    /// them gets accepted once there is no room left.
    resets: FuturesUnordered<ResetWatch>,
}

impl Pending {
    fn new() -> Self {
        Self {
            requests: HashMap::new(),
            deadlines: DelayQueue::new(),
            resets: FuturesUnordered::new(),
        }
    }

    /// A stream of a request which waits for the other one, and the watch
    /// for its reset, see [`Self::resets`].
    fn wait(request_id: &Arc<str>, stream: Stream) -> (Waiting, ResetWatch) {
        let waiting = CancellationToken::new();
        let (reset, stopped) = (stream.reset_watch(), waiting.clone());
        let request_id = request_id.clone();
        let watch = Box::pin(async move {
            select! {
                biased;
                () = stopped.cancelled() => None,
                () = reset => Some(request_id),
            }
        });
        let stream = Waiting {
            stream,
            _watch: waiting.drop_guard(),
        };
        (stream, watch)
    }

    /// The request, created with its deadline if it is new.
    fn get_or_insert(&mut self, request_id: &Arc<str>) -> &mut PendingRequest {
        let Self {
            requests,
            deadlines,
            ..
        } = self;
        requests
            .entry(request_id.clone())
            .or_insert_with(|| PendingRequest {
                data: None,
                error: None,
                deadline: deadlines.insert(request_id.clone(), STREAM_CREATION_TIMEOUT),
            })
    }

    /// Remove a request, with its deadline.
    fn remove(&mut self, request_id: &str) -> Option<PendingRequest> {
        let request = self.requests.remove(request_id)?;
        self.deadlines.remove(&request.deadline);
        Some(request)
    }

    /// Let go of the streams of a request which the client reset, and of
    /// the request once it has no stream left.
    fn remove_reset(&mut self, request_id: &str) {
        let Some(request) = self.requests.get_mut(request_id) else {
            return;
        };
        request.data.take_if(|stream| stream.is_reset());
        request.error.take_if(|stream| stream.is_reset());
        if request.data.is_none() && request.error.is_none() {
            self.remove(request_id);
        }
    }
}

/// The sockets for a connection into the pod, one per address family.
type Sockets = Vec<(IpAddr, io::Result<StdTcpStream>)>;

/// The requests for sockets to the thread in the network namespace.
type SocketRequests = std_mpsc::Sender<oneshot::Sender<Result<Sockets>>>;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Creates the sockets of a session in its network namespace, which stay in
/// the namespace they got created in. The runtime connects them. A thread
/// enters the namespace and serves the connections which follow within the
/// linger time, then it ends and leaves the namespace, at once when the
/// session ends. Failures are not kept, every connection which fails gets its
/// own error.
struct Namespace {
    path: PathBuf,
    linger: Duration,

    /// The requests of the thread which got started last.
    requests: Mutex<Option<SocketRequests>>,

    /// What the connections which get established hold, see
    /// [`CONNECT_BUDGET`].
    connect_budget: Budget,

    /// How many threads got started and ended.
    #[cfg(test)]
    threads: AtomicUsize,
    #[cfg(test)]
    ended: Arc<AtomicUsize>,
}

impl Namespace {
    fn new(path: PathBuf) -> Arc<Self> {
        Self::with_linger(path, SOCKET_THREAD_LINGER)
    }

    fn with_linger(path: PathBuf, linger: Duration) -> Arc<Self> {
        Arc::new(Self {
            path,
            linger,
            requests: Mutex::default(),
            connect_budget: Budget::new(CONNECT_BUDGET),
            #[cfg(test)]
            threads: AtomicUsize::new(0),
            #[cfg(test)]
            ended: Arc::default(),
        })
    }

    async fn sockets(&self) -> Result<Sockets> {
        // A thread which ended before it took the request gets replaced
        // once.
        for _ in 0..2 {
            let (tx, rx) = oneshot::channel();
            self.send(tx)?;
            if let Ok(sockets) = rx.await {
                return sockets;
            }
        }
        bail!("socket thread ended")
    }

    /// Send a request to the thread, which gets started if it ended.
    fn send(&self, reply: oneshot::Sender<Result<Sockets>>) -> Result<()> {
        let running = self.lock().clone();
        let reply = match running {
            Some(requests) => match requests.send(reply) {
                Ok(()) => return Ok(()),
                Err(std_mpsc::SendError(reply)) => reply,
            },
            None => reply,
        };
        let requests = self.start()?;
        requests
            .send(reply)
            .map_err(|_| anyhow::anyhow!("socket thread ended"))?;
        *self.lock() = Some(requests);
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Option<SocketRequests>> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start(&self) -> Result<SocketRequests> {
        let (tx, rx) = std_mpsc::channel();
        let (path, linger) = (self.path.clone(), self.linger);
        #[cfg(test)]
        let ended = self.ended.clone();
        thread::Builder::new()
            .name("portforward".into())
            .stack_size(SOCKET_THREAD_STACK_SIZE)
            .spawn(move || {
                Self::serve(&path, linger, &rx);
                #[cfg(test)]
                ended.fetch_add(1, Ordering::Relaxed);
            })
            .context("spawn socket thread")?;
        #[cfg(test)]
        self.threads.fetch_add(1, Ordering::Relaxed);
        Ok(tx)
    }

    /// Create sockets until no request arrived for the linger time or the
    /// session is gone. The thread enters the namespace for its first
    /// request, a request which finds it outside of the namespace tries again
    /// and gets its own error.
    fn serve(
        path: &Path,
        linger: Duration,
        requests: &std_mpsc::Receiver<oneshot::Sender<Result<Sockets>>>,
    ) {
        let mut entered = false;
        while let Ok(reply) = requests.recv_timeout(linger) {
            if !entered {
                if let Err(e) = Self::enter(path) {
                    let _ = reply.send(Err(e));
                    continue;
                }
                entered = true;
            }
            let _ = reply.send(Ok(Self::create_sockets()));
        }
    }

    /// Enter the network namespace with the current thread.
    fn enter(path: &Path) -> Result<()> {
        let netns = File::open(path)
            .with_context(|| format!("open network namespace {}", path.display()))?;
        // Entering the own namespace needs privileges as well, which a server
        // for a host network pod or in tests may not have.
        let current = fs::metadata("/proc/thread-self/ns/net").context("stat network namespace")?;
        let target = netns.metadata().context("stat target network namespace")?;
        if (current.dev(), current.ino()) != (target.dev(), target.ino()) {
            setns(&netns, CloneFlags::CLONE_NEWNET)
                .with_context(|| format!("enter network namespace {}", path.display()))?;
        }
        Ok(())
    }

    /// A socket of each family, for one connection. The IPv6 one is only
    /// needed if IPv4 fails, creating it right away costs less than another
    /// round trip into the namespace thread.
    fn create_sockets() -> Sockets {
        [
            (AddressFamily::Inet, IpAddr::V4(Ipv4Addr::LOCALHOST)),
            (AddressFamily::Inet6, IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ]
        .into_iter()
        .map(|(family, ip)| {
            let socket = socket(
                family,
                SockType::Stream,
                SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
                None,
            )
            .map(StdTcpStream::from)
            .map_err(io::Error::from);
            (ip, socket)
        })
        .collect()
    }
}

impl PortForward {
    pub fn new(netns_path: PathBuf, idle_timeout: Option<Duration>) -> Self {
        Self {
            netns_path,
            idle_timeout,
            ping_interval: Some(PING_INTERVAL),
        }
    }

    /// Serve SPDY without pinging the client.
    #[cfg(test)]
    fn without_pings(mut self) -> Self {
        self.ping_interval = None;
        self
    }

    /// Serve port forward requests on the upgraded connection until it ends.
    pub async fn serve_spdy<T>(self, io: T)
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let mut connection =
            Connection::serve(io, self.idle_timeout, self.ping_interval, Self::accept);
        let namespace = Namespace::new(self.netns_path);
        let mut pending = Pending::new();

        loop {
            select! {
                Some(reset) = pending.resets.next() => if let Some(request_id) = reset {
                    pending.remove_reset(&request_id);
                },
                stream = connection.accept() => match stream {
                    Some(stream) => Self::add(&mut pending, stream, &namespace),
                    None => break,
                },
                Some(expired) = pending.deadlines.next() => {
                    Self::expire(&mut pending, expired.into_inner());
                }
            }
        }
        debug!("Port forward connection ended");
    }

    /// The ports a channel protocol request forwards, by its query. The
    /// `port` parameter may repeat and hold several ports separated by
    /// commas.
    pub fn query_ports(query: Option<&str>) -> Result<Vec<u16>> {
        let mut ports = vec![];
        for pair in query.unwrap_or_default().split('&') {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if Self::percent_decode(name) != PORT_QUERY {
                continue;
            }
            let value = Self::percent_decode(value);
            ensure!(
                !value.is_empty(),
                "query parameter {PORT_QUERY:?} cannot be empty"
            );
            for port in value.split(',') {
                let port: u16 = port
                    .parse()
                    .with_context(|| format!("unable to parse {port:?} as a port"))?;
                ensure!(port > 0, "port {port} must be > 0");
                ports.push(port);
            }
        }
        ensure!(
            !ports.is_empty(),
            "query parameter {PORT_QUERY:?} is required"
        );
        ensure!(
            ports.len() <= MAX_CHANNEL_PORTS,
            "at most {MAX_CHANNEL_PORTS} ports can be forwarded at once"
        );
        Ok(ports)
    }

    /// Decode a query component, invalid escapes stay as they are.
    fn percent_decode(s: &str) -> String {
        percent_decode_str(&s.replace('+', " "))
            .decode_utf8_lossy()
            .into_owned()
    }

    /// Serve port forwarding on the upgraded WebSocket until it ends. The
    /// channel protocol forwards the ports, the tunnel ignores them.
    pub async fn serve_websocket(
        self,
        socket: WebSocket,
        protocol: WebSocketProtocol,
        ports: Vec<u16>,
    ) {
        match protocol {
            WebSocketProtocol::SpdyTunnel => {
                let (server, tunnel) = tokio::io::duplex(TUNNEL_BUFFER);
                let spdy = tokio::spawn(self.serve_spdy(server));
                Self::tunnel(socket, tunnel).await;
                let _ = spdy.await;
            }
            WebSocketProtocol::Channels { base64 } => {
                self.serve_channels(socket, ports, base64).await;
            }
        }
        debug!("Port forward WebSocket ended");
    }

    /// Carry the bytes of the SPDY connection in binary messages, until
    /// either side ends.
    async fn tunnel<T>(socket: WebSocketStream<web::ClientIo<T>>, connection: DuplexStream)
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let discard = socket.get_ref().discarder();
        let (mut sink, mut messages) = socket.split();
        let (mut from_server, mut to_server) = tokio::io::split(connection);
        // Cancelled once the client sent a message larger than the server
        // takes, the close tells it why.
        let too_large = CancellationToken::new();

        // The sink, once the close went out.
        let mut to_client = tokio::spawn({
            let too_large = too_large.clone();
            async move {
                let mut buf = BytesMut::new();
                loop {
                    buf.reserve(TUNNEL_BUFFER);
                    match from_server.read_buf(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if sink
                                .send(Message::Binary(buf.split().freeze()))
                                .await
                                .is_err()
                            {
                                return None;
                            }
                        }
                    }
                }
                let close = web::close(too_large.is_cancelled(), "");
                sink.send(close).await.is_ok().then_some(sink)
            }
        });

        let from_client = async {
            while let Some(message) = messages.next().await {
                match message {
                    Ok(Message::Binary(data)) => {
                        // The SPDY connection ended, its close goes to the
                        // client, which still gets read until it answered,
                        // for a bounded time.
                        if to_server.write_all(&data).await.is_err() {
                            web::await_close_answer(&mut messages).await;
                            break;
                        }
                    }
                    Ok(Message::Text(_)) => {
                        debug!("Ending port forward tunnel, the client sent a text message");
                        break;
                    }
                    Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                    Err(e) if web::is_too_large(&e) => {
                        debug!("Ending port forward tunnel: {e}");
                        too_large.cancel();
                        // It may still be busy writing the rest, which holds
                        // up the close for it.
                        discard.discard();
                        break;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                }
            }
            // Ends the SPDY connection, which ends the output as well.
            let _ = to_server.shutdown().await;
        };

        let sink = {
            let mut from_client = pin!(from_client);
            let closed_first = select! {
                () = &mut from_client => None,
                sink = &mut to_client => Some(sink.ok().flatten()),
            };
            match closed_first {
                None => web::join_or_abort(to_client).await.flatten(),
                // The SPDY connection ended, the client answers the close.
                Some(Some(sink)) => {
                    let _ = time::timeout(web::CLOSE_TIMEOUT, from_client).await;
                    Some(sink)
                }
                Some(None) => None,
            }
        };
        // A client which sent a message which was too large cannot answer.
        if let Some(sink) = sink.filter(|_| too_large.is_cancelled()) {
            web::drain(sink, messages).await;
        }
    }

    /// Serve the channel protocol: forward a connection per port until all
    /// of them ended, the client went away or the WebSocket was idle.
    async fn serve_channels(self, socket: WebSocket, ports: Vec<u16>, base64: bool) {
        let discard = socket.get_ref().discarder();
        let (mut sink, mut messages) = socket.split();
        let (output_tx, mut output_rx) = mpsc::channel(OUTPUT_QUEUE);
        let activity = Activity::new();
        let too_large = CancellationToken::new();

        // On its own, as a port which does not take its input stops reading
        // the WebSocket.
        let idle = CancellationToken::new();
        // Ends the watch however the session ends.
        let _idle_guard = idle.clone().drop_guard();
        tokio::spawn({
            let (activity, idle, timeout) = (activity.clone(), idle.clone(), self.idle_timeout);
            async move {
                if activity
                    .until_idle(timeout, idle.cancelled(), || {})
                    .await
                    .is_none()
                {
                    debug!("Closing idle port forward WebSocket");
                    idle.cancel();
                }
            }
        });

        let (writer_activity, writer_too_large) = (activity.clone(), too_large.clone());
        // The sink, once the close went out.
        let writer = tokio::spawn(async move {
            while let Some(message) = output_rx.recv().await {
                if sink.send(message).await.is_err() {
                    return None;
                }
                writer_activity.touch();
            }
            let close = web::close(writer_too_large.is_cancelled(), "");
            sink.send(close).await.is_ok().then_some(sink)
        });

        let namespace = Namespace::new(self.netns_path);
        let budget = Budget::new(CHANNEL_BUFFER);
        let mut inputs = Vec::with_capacity(ports.len());
        let mut forwards = JoinSet::new();
        for (i, port) in ports.into_iter().enumerate() {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "at most MAX_CHANNEL_PORTS ports"
            )]
            let output = ChannelOutput {
                messages: output_tx.clone(),
                base64,
                data: (i * 2) as u8,
                error: (i * 2 + 1) as u8,
            };
            // The client learns the port of each channel from its first
            // message.
            let prefix = port.to_le_bytes();
            if output.send(output.data, &prefix).await.is_err()
                || output.send(output.error, &prefix).await.is_err()
            {
                return;
            }
            let (input_tx, input_rx) = mpsc::channel(CHANNEL_QUEUE);
            inputs.push(input_tx);
            forwards.spawn(Self::forward_channel(
                namespace.clone(),
                port,
                input_rx,
                output,
            ));
        }
        drop(output_tx);

        let mut client_gone = false;
        loop {
            select! {
                // Nothing gets dispatched after the idle timeout, and the
                // session ends with its last forward, also while the client
                // keeps sending.
                biased;
                () = idle.cancelled() => break,
                done = forwards.join_next() => if done.is_none() {
                    break;
                },
                message = messages.next() => {
                    // Pings and pongs only keep the WebSocket alive, the
                    // heartbeat of a client does not keep it from going idle.
                    let data = match message {
                        Some(Ok(Message::Binary(data))) => data,
                        Some(Ok(Message::Text(text))) => Bytes::from(text),
                        Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                        // The client cannot be read anymore, it gets the
                        // close only. Like for Kubernetes, that ends the
                        // session, the error channels of the ports are not
                        // about it.
                        Some(Err(e)) if web::is_too_large(&e) => {
                            debug!("Ending port forward WebSocket: {e}");
                            too_large.cancel();
                            // It may still be busy writing the rest, which
                            // holds up the close for it.
                            discard.discard();
                            client_gone = true;
                            break;
                        }
                        Some(Ok(Message::Close(_)) | Err(_)) | None => {
                            client_gone = true;
                            break;
                        }
                    };
                    activity.touch();
                    Self::dispatch(&inputs, &budget, base64, data, &idle).await;
                    // A dispatch which ended with the idle timeout is no
                    // activity.
                    if idle.is_cancelled() {
                        break;
                    }
                    // A port which takes its input slowly keeps the WebSocket
                    // active.
                    activity.touch();
                }
            }
        }

        // The ports still forwarded lose their client.
        forwards.abort_all();
        while forwards.join_next().await.is_some() {}
        // Like the error of a port which failed before its input came, the
        // client has to get what the server sent before.
        if too_large.is_cancelled() {
            if let Some(Some(sink)) = web::join_or_abort(writer).await {
                web::drain(sink, messages).await;
            }
        } else if client_gone {
            web::join_or_abort(writer).await;
        } else {
            web::finish(writer, &mut messages).await;
        }
    }

    /// Hand a message of the client to the connection of its channel. Only
    /// the data channels, the even ones, take input. Waits while a connected
    /// port does not take its input or the ports hold all of the budget,
    /// which stops reading the WebSocket like the reference implementation
    /// does, the protocol has no flow control. Connecting ports take it, see
    /// [`Self::drain_until`], and the idle timeout still ends the wait.
    async fn dispatch(
        inputs: &[mpsc::Sender<Held>],
        budget: &Budget,
        base64: bool,
        message: Bytes,
        idle: &CancellationToken,
    ) {
        let Some((channel, data)) = channel::decode(base64, message) else {
            return;
        };
        let Some(input) = inputs
            .get(usize::from(channel / 2))
            .filter(|_| channel % 2 == 0)
        else {
            trace!("Dropping port forward message for channel {channel}");
            return;
        };
        if !data.is_empty() {
            select! {
                // A connection which ended does not take input anymore.
                _ = async { input.send(budget.hold(data).await).await } => {}
                // Also while waiting for the budget.
                () = input.closed() => {}
                () = idle.cancelled() => {}
            }
        }
    }

    /// Forward the connection of a port, the error channel reports a failure.
    async fn forward_channel(
        namespace: Arc<Namespace>,
        port: u16,
        mut input: mpsc::Receiver<Held>,
        output: ChannelOutput,
    ) {
        debug!("Forwarding port {port} over WebSocket");
        let result = match Self::establish(&namespace, port, &mut input).await {
            Ok((connection, early)) => {
                Self::copy_channel(early, &mut input, &output, connection).await
            }
            Err(e) => Err(e),
        };
        // What the client sent gives its budget back, and the WebSocket gets
        // read on while the error waits for a client which does not read.
        drop(input);
        if let Err(e) = result {
            let message = format!("error forwarding port {port}: {e:#}");
            debug!("Port forward over WebSocket: {message}");
            let _ = output.send(output.error, message.as_bytes()).await;
        }
    }

    /// Copy between the channels of a port and its connection in the pod,
    /// until the pod closed the connection or the client went away. The
    /// protocol has no way to close a channel, the input after the pod closed
    /// the connection does not matter.
    async fn copy_channel(
        early: Early,
        input: &mut mpsc::Receiver<Held>,
        output: &ChannelOutput,
        connection: TcpStream,
    ) -> Result<()> {
        let (mut from_container, mut to_container) = connection.into_split();
        let upstream = Self::write_input(early, input, &mut to_container);

        let downstream = async {
            let mut buf = vec![0; READ_BUFFER];
            loop {
                let read = from_container
                    .read(&mut buf)
                    .await
                    .context("read from the pod")?;
                if read == 0 {
                    return anyhow::Ok(());
                }
                if output.send(output.data, &buf[..read]).await.is_err() {
                    // The client went away.
                    return Ok(());
                }
            }
        };

        select! {
            result = downstream => result,
            result = upstream => result,
        }
    }

    /// Whether a stream is part of a valid request, by its headers.
    fn accept(headers: &Headers) -> Result<()> {
        Self::port(headers)?;
        match headers.get(STREAM_TYPE_HEADER) {
            Some(STREAM_TYPE_DATA | STREAM_TYPE_ERROR) => Ok(()),
            Some(typ) => bail!("invalid stream type {:?}", ClientStr(typ)),
            None => bail!("{STREAM_TYPE_HEADER:?} header is required"),
        }
    }

    fn port(headers: &Headers) -> Result<u16> {
        let port = headers
            .get(PORT_HEADER)
            .with_context(|| format!("{PORT_HEADER:?} header is required"))?;
        let port: u16 = port
            .parse()
            .with_context(|| format!("unable to parse {:?} as a port", ClientStr(port)))?;
        ensure!(port > 0, "port {port} must be > 0");
        Ok(port)
    }

    fn is_error_stream(stream: &Stream) -> bool {
        stream.headers().get(STREAM_TYPE_HEADER) == Some(STREAM_TYPE_ERROR)
    }

    /// The request a stream belongs to. Old clients send no ID, their
    /// requests use the IDs of the error streams, the data streams follow
    /// them.
    fn request_id(stream: &Stream) -> Arc<str> {
        match stream.headers().get(REQUEST_ID_HEADER) {
            Some(id) if !id.is_empty() => id.into(),
            _ if Self::is_error_stream(stream) => stream.id().to_string().into(),
            _ => (i64::from(stream.id()) - 2).to_string().into(),
        }
    }

    /// Add a stream to its request, which starts once both streams arrived.
    fn add(pending: &mut Pending, stream: Stream, namespace: &Arc<Namespace>) {
        let request_id = Self::request_id(&stream);
        let is_error = Self::is_error_stream(&stream);
        let (stream, watch) = Pending::wait(&request_id, stream);
        let request = pending.get_or_insert(&request_id);
        // A stream the client reset does not count, also before the reset
        // got handled.
        request.data.take_if(|stream| stream.is_reset());
        request.error.take_if(|stream| stream.is_reset());
        let slot = if is_error {
            &mut request.error
        } else {
            &mut request.data
        };
        if slot.is_some() {
            // Dropping it resets it.
            debug!(
                "Rejecting stream {} of port forward request {:?}, the request has one of its type already",
                stream.stream.id(),
                ClientStr(&request_id)
            );
            return;
        }
        *slot = Some(stream);

        if request.data.is_none() || request.error.is_none() {
            pending.resets.push(watch);
            return;
        }
        if let Some(PendingRequest {
            data: Some(data),
            error: Some(error),
            ..
        }) = pending.remove(&request_id)
        {
            tokio::spawn(Self::forward(
                request_id,
                data.stream,
                error.stream,
                namespace.clone(),
            ));
        }
    }

    /// Fail a request whose second stream did not arrive in time.
    fn expire(pending: &mut Pending, request_id: Arc<str>) {
        let Some(mut request) = pending.requests.remove(&request_id) else {
            return;
        };
        debug!(
            "Port forward request {:?} timed out waiting for streams",
            ClientStr(&request_id)
        );
        if let Some((_, mut error)) = request.error.take().map(|error| error.stream.split()) {
            let message = format!("request {request_id}: timed out waiting for streams");
            tokio::spawn(async move {
                let _ = error.send(message.into()).await;
                let _ = error.close().await;
            });
        }
    }

    /// Forward a single connection, the error stream reports a failure.
    async fn forward(request_id: Arc<str>, data: Stream, error: Stream, namespace: Arc<Namespace>) {
        let port = Self::port(data.headers()).unwrap_or_default();
        let id = ClientStr(&request_id);
        debug!("Forwarding port {port} for request {id:?}");
        // The server does not read the error stream, what a client sends on
        // it gets dropped instead of holding up the connection.
        let (_, mut error) = error.split();

        let (mut from_client, to_client) = data.split();
        let result = match Self::establish(&namespace, port, &mut from_client).await {
            Ok((connection, early)) => Self::copy(early, from_client, to_client, connection).await,
            Err(e) => {
                // Dropping the sending side resets the data stream, what the
                // client sent on it gives its budget back.
                drop((from_client, to_client));
                Err(e)
            }
        };
        match result {
            Ok(()) => debug!("Done forwarding port {port} for request {id:?}"),
            Err(e) => {
                let message = format!("error forwarding port {port}: {e:#}");
                debug!("Port forward request {id:?}: {message}");
                if let Err(e) = error.send(message.into()).await {
                    debug!("Unable to report port forward error: {e:#}");
                }
            }
        }
        if let Err(e) = error.close().await {
            debug!("Unable to close port forward error stream: {e:#}");
        }
    }

    /// Connect to the port in the pod while taking what the client sends
    /// meanwhile.
    async fn establish(
        namespace: &Namespace,
        port: u16,
        input: &mut impl Input,
    ) -> Result<(TcpStream, Early)> {
        Self::drain_until(
            Self::connect(namespace, port),
            &namespace.connect_budget,
            input,
        )
        .await
    }

    /// Wait for the connection to the pod while taking what the client sends
    /// meanwhile, which may take until the connect timeout. The data moves
    /// from the byte budget of the client into the connect budget, so that
    /// the client can go on with its other connections. Without room in the
    /// connect budget no more data gets taken, see [`CONNECT_BUDGET`]. A
    /// client which sends more than [`CONNECT_BUFFER`] fails the forwarding.
    async fn drain_until<T>(
        connect: impl Future<Output = Result<T>>,
        connect_budget: &Budget,
        input: &mut impl Input,
    ) -> Result<(T, Early)> {
        let mut connect = pin!(connect);
        let mut early = Early::default();
        let mut len = 0;
        // Data which got taken and waits for room in the connect budget.
        let mut taken: Option<Held> = None;
        loop {
            select! {
                result = &mut connect => {
                    early.last = taken;
                    return Ok((result?, early));
                }
                held = async {
                    match &taken {
                        Some(data) => {
                            let cost = early.data.cost(data.len());
                            connect_budget.hold_as(Bytes::clone(data), cost).await
                        }
                        None => future::pending().await,
                    }
                } => {
                    // The data leaves the byte budget of the client.
                    taken = None;
                    early.data.push(held);
                }
                data = input.recv(), if taken.is_none() && !early.ended => match data {
                    Some(data) => {
                        len += data.len();
                        ensure!(
                            len <= CONNECT_BUFFER,
                            "the client sent more than {CONNECT_BUFFER} bytes before the connection got established"
                        );
                        taken = Some(data);
                    }
                    None => early.ended = true,
                },
            }
        }
    }

    /// Copy between the data stream and the connection in the pod, until
    /// both sides closed or either reset. A failure resets the data stream.
    async fn copy(
        early: Early,
        mut from_client: StreamReceiver,
        mut to_client: StreamSender,
        connection: TcpStream,
    ) -> Result<()> {
        let (mut from_container, mut to_container) = connection.into_split();

        let upstream = async {
            Self::write_input(early, &mut from_client, &mut to_container).await?;
            if !from_client.is_reset() {
                to_container
                    .shutdown()
                    .await
                    .context("close the connection to the pod")?;
            }
            anyhow::Ok(())
        };

        let downstream = async {
            let mut buf = BytesMut::new();
            loop {
                buf.reserve(READ_BUFFER);
                let mut limited = (&mut buf).limit(READ_BUFFER);
                let read = select! {
                    read = from_container.read_buf(&mut limited) => read.context("read from the pod")?,
                    // Nothing takes the output anymore.
                    () = to_client.reset_by_peer() => return Ok(()),
                };
                if read == 0 {
                    break;
                }
                let data: Bytes = buf.split().freeze();
                if let Err(e) = to_client.send(data).await {
                    return Self::unless_reset(&to_client, e);
                }
            }
            match to_client.close().await {
                Ok(()) => Ok(()),
                Err(e) => Self::unless_reset(&to_client, e),
            }
        };

        let mut upstream = pin!(upstream);
        let mut downstream = pin!(downstream);
        select! {
            result = &mut downstream => {
                result?;
                if time::timeout(CLOSE_GRACE_PERIOD, upstream).await.is_err() {
                    debug!("Client did not close its side of the connection");
                }
                Ok(())
            }
            // The client closed its side, the pod may still answer.
            result = &mut upstream => {
                result?;
                downstream.await
            }
        }
    }

    /// A stream the client reset or whose connection ended is no failure.
    fn unless_reset(sender: &StreamSender, e: anyhow::Error) -> Result<()> {
        if sender.is_reset() { Ok(()) } else { Err(e) }
    }

    /// Write what the client sends to the pod until the client is done,
    /// starting with what it sent while the connection got established,
    /// which gives its budget back once it got written.
    async fn write_input(
        early: Early,
        input: &mut impl Input,
        to_container: &mut OwnedWriteHalf,
    ) -> Result<()> {
        let Early { data, last, ended } = early;
        to_container
            .write_all(&data)
            .await
            .context("write to the pod")?;
        drop(data);
        if let Some(last) = last {
            to_container
                .write_all(&last)
                .await
                .context("write to the pod")?;
        }
        if !ended {
            while let Some(data) = input.recv().await {
                to_container
                    .write_all(&data)
                    .await
                    .context("write to the pod")?;
            }
        }
        Ok(())
    }

    /// Connect to the port in the network namespace of the pod. The pod may
    /// listen on either family, they get tried one after the other like CRI-O
    /// does.
    async fn connect(namespace: &Namespace, port: u16) -> Result<TcpStream> {
        let mut errors = vec![];
        for (ip, socket) in namespace.sockets().await? {
            let result = match socket {
                Ok(socket) => time::timeout(
                    CONNECT_TIMEOUT,
                    TcpSocket::from_std_stream(socket).connect((ip, port).into()),
                )
                .await
                .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into())),
                Err(e) => Err(e),
            };
            match result {
                Ok(connection) => return Ok(connection),
                Err(e) => errors.push(format!("{ip}: {e}")),
            }
        }
        bail!(
            "failed to connect to localhost:{port} inside network namespace {}: {}",
            namespace.path.display(),
            errors.join(", ")
        )
    }
}

/// The entry points of the fuzz targets.
#[cfg(feature = "fuzzing")]
pub mod fuzzing {
    use super::{
        MAX_CHANNEL_PORTS, PORT_HEADER, PortForward, REQUEST_ID_HEADER, STREAM_CREATION_TIMEOUT,
        STREAM_TYPE_DATA, STREAM_TYPE_ERROR,
    };
    use crate::{spdy::fuzzing::client_frames, stream_protocol::STREAM_TYPE_HEADER};
    use std::{
        pin::{Pin, pin},
        sync::{Arc, Mutex, PoisonError},
        task::{Context, Poll},
    };
    use tokio::{
        io::{self, AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
        runtime, select, task,
        time::{self, Duration, Instant},
    };

    /// How long the server may go without reading the client before the
    /// session counts as hung. A stream which does not take its data stops
    /// reading the client until its request expires, at most
    /// [`STREAM_CREATION_TIMEOUT`] after the read which created it, and the
    /// client waits a little longer than that before it closes the
    /// connection.
    const MAX_STALL: Duration = STREAM_CREATION_TIMEOUT.saturating_mul(2);

    /// The server side of the connection, which notes when the server read
    /// from the client the last time.
    struct Watched {
        io: DuplexStream,
        last_read: Arc<Mutex<Instant>>,
    }

    impl AsyncRead for Watched {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let poll = Pin::new(&mut this.io).poll_read(cx, buf);
            if poll.is_ready() {
                *this
                    .last_read
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Instant::now();
            }
            poll
        }
    }

    impl AsyncWrite for Watched {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().io).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
        }
    }

    /// Parse the query of a channel protocol request, the ports it gives
    /// are the ones a session can forward.
    pub fn query_ports(query: &str) {
        if let Ok(ports) = PortForward::query_ports(Some(query)) {
            assert!(!ports.is_empty());
            assert!(ports.len() <= MAX_CHANNEL_PORTS);
            assert!(!ports.contains(&0));
        }
    }

    /// Serve a SPDY port forward session to a client which sends the frames
    /// the input describes, like the SPDY connection target, then closes the
    /// connection. The streams pair up into requests, which fail to enter
    /// the network namespace, so that only the pairing, the resets and the
    /// errors run. The session has to end with the client. The namespace
    /// thread answers in real time while the clock is paused, so whether an
    /// error reaches the client before it closes varies between runs.
    ///
    /// The server may stop reading the client while a stream waits for the
    /// other one of its request, until the request expires. The client
    /// waits for the server meanwhile, and a request which arrives after
    /// one expired may stop the server again. So the session as a whole
    /// may take several request timeouts, but the server must never go
    /// [`MAX_STALL`] without reading the client until it ended.
    pub fn spdy(input: &[u8]) {
        let frames = client_frames(input, |next| {
            // Mostly valid streams, so that requests pair up.
            let typ = match next() % 8 {
                0..4 => STREAM_TYPE_DATA,
                4..7 => STREAM_TYPE_ERROR,
                _ => "other",
            };
            let port = match next() % 8 {
                0..6 => "80",
                6 => "0",
                _ => "http",
            };
            // Old clients send no request ID, their streams pair by their
            // IDs.
            let id = next();
            let id = if id < 64 {
                String::new()
            } else {
                (id % 4).to_string()
            };
            [
                (STREAM_TYPE_HEADER, typ),
                (PORT_HEADER, port),
                (REQUEST_ID_HEADER, id.as_str()),
            ]
            .into()
        });
        // The paused clock gets to the deadlines of the requests at once.
        let runtime = runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (client, server) = io::duplex(64 * 1024);
            let (mut from_server, mut to_server) = io::split(client);
            let client = task::spawn(async move {
                // The connection stays open until the requests whose second
                // stream did not come expired, the streams of a connection
                // which ended count as reset.
                let write = async {
                    let _ = to_server.write_all(&frames).await;
                    time::sleep(STREAM_CREATION_TIMEOUT + Duration::from_secs(1)).await;
                    let _ = to_server.shutdown().await;
                };
                let mut sink = io::sink();
                let _ = tokio::join!(write, io::copy(&mut from_server, &mut sink));
            });
            let session = PortForward {
                netns_path: "/proc/self/ns/none".into(),
                idle_timeout: None,
                ping_interval: None,
            };
            let last_read = Arc::new(Mutex::new(Instant::now()));
            let server = Watched {
                io: server,
                last_read: last_read.clone(),
            };
            let mut served = pin!(async {
                session.serve_spdy(server).await;
                let _ = client.await;
            });
            let last_read = || *last_read.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                select! {
                    biased;
                    () = &mut served => break,
                    () = time::sleep_until(last_read() + MAX_STALL) => assert!(
                        last_read().elapsed() < MAX_STALL,
                        "the session ended with the client"
                    ),
                }
            }
        });
    }
}

/// What the tests of the protocols on top need.
#[cfg(test)]
pub mod testing {
    use anyhow::Result;
    use std::net::Ipv4Addr;
    use tokio::net::TcpSocket;

    /// A socket bound to a port which does not listen, so that connecting to
    /// the port fails. It keeps the port, a closed one could be taken by
    /// another test meanwhile.
    pub fn closed_port() -> Result<(TcpSocket, u16)> {
        let socket = TcpSocket::new_v4()?;
        socket.bind((Ipv4Addr::LOCALHOST, 0).into())?;
        let port = socket.local_addr()?.port();
        Ok((socket, port))
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::closed_port, *};
    use crate::spdy::{
        MAX_STREAMS,
        testing::{Client, Frame},
    };
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::protocol::Role;

    /// The network namespace of the tests, which needs no privileges to
    /// connect in.
    const NETNS: &str = "/proc/self/ns/net";

    fn serve() -> Result<Client> {
        let (client, server) = Client::pair()?;
        tokio::spawn(
            PortForward::new(NETNS.into(), None)
                .without_pings()
                .serve_spdy(server),
        );
        Ok(client)
    }

    fn headers(typ: &str, port: u16) -> Headers {
        let port = port.to_string();
        [("streamType", typ), ("port", &port), ("requestID", "0")].into()
    }

    /// Open the streams of a request, the error stream closed right away
    /// like clients do.
    async fn request(client: &mut Client, port: u16) -> Result<()> {
        client.open(1, headers(STREAM_TYPE_ERROR, port)).await?;
        client
            .send(Frame::Data {
                stream_id: 1,
                fin: true,
                data: Bytes::new(),
            })
            .await?;
        client.open(3, headers(STREAM_TYPE_DATA, port)).await?;
        for stream_id in [1, 3] {
            assert_eq!(
                client.frame().await?,
                Some(Frame::SynReply {
                    stream_id,
                    headers: Headers::default()
                })
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn forwards_a_connection() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let mut client = serve()?;
        request(&mut client, port).await?;

        client
            .send(Frame::Data {
                stream_id: 3,
                fin: true,
                data: Bytes::from_static(b"request"),
            })
            .await?;
        let (mut connection, _) = listener.accept().await?;
        let mut request = vec![];
        // The client closed its side, which ends the input of the pod.
        connection.read_to_end(&mut request).await?;
        assert_eq!(request, b"request");
        connection.write_all(b"response").await?;
        drop(connection);

        assert_eq!(
            client.frame().await?,
            Some(Frame::Data {
                stream_id: 3,
                fin: false,
                data: Bytes::from_static(b"response")
            })
        );
        // Both streams get closed, without an error.
        let mut closed = vec![];
        for _ in 0..2 {
            match client.frame().await? {
                Some(Frame::Data {
                    stream_id,
                    fin: true,
                    data,
                }) if data.is_empty() => closed.push(stream_id),
                frame => panic!("unexpected frame {frame:?}"),
            }
        }
        closed.sort();
        assert_eq!(closed, [1, 3]);
        Ok(())
    }

    #[tokio::test]
    async fn data_on_the_error_stream_gets_dropped() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let mut client = serve()?;
        client.open(1, headers(STREAM_TYPE_ERROR, port)).await?;
        client.open(3, headers(STREAM_TYPE_DATA, port)).await?;
        let (mut connection, _) =
            time::timeout(Duration::from_secs(5), listener.accept()).await??;

        // More than the connection holds, which nothing takes.
        let request = async {
            for _ in 0..80 {
                client
                    .send(Frame::Data {
                        stream_id: 1,
                        fin: false,
                        data: Bytes::from(vec![0; 64 * 1024]),
                    })
                    .await?;
            }
            client
                .send(Frame::Data {
                    stream_id: 3,
                    fin: true,
                    data: Bytes::from_static(b"request"),
                })
                .await?;
            let mut request = vec![];
            connection.read_to_end(&mut request).await?;
            anyhow::Ok(request)
        };
        let request = time::timeout(Duration::from_secs(5), request)
            .await
            .context("the connection is held up")??;
        assert_eq!(request, b"request");
        Ok(())
    }

    #[tokio::test]
    async fn failed_channels_let_go_of_their_input() -> Result<()> {
        let (_socket, port) = closed_port()?;
        let budget = Budget::new(CHANNEL_BUFFER);
        let (input_tx, input) = mpsc::channel(CHANNEL_QUEUE);
        // A client which does not read, the error waits.
        let (messages, mut output) = mpsc::channel(1);
        messages.send(Message::Binary(Bytes::new())).await?;
        let forward = tokio::spawn(PortForward::forward_channel(
            Namespace::new(NETNS.into()),
            port,
            input,
            ChannelOutput {
                messages,
                base64: false,
                data: 0,
                error: 1,
            },
        ));

        // The input does not wait for the error, which still comes.
        let sent = async {
            for _ in 0..2 * CHANNEL_QUEUE {
                let data = budget.hold(Bytes::from(vec![0; READ_BUFFER])).await;
                if input_tx.send(data).await.is_err() {
                    break;
                }
            }
        };
        time::timeout(Duration::from_secs(5), sent)
            .await
            .context("input still taken")?;
        time::timeout(Duration::from_secs(5), input_tx.closed())
            .await
            .context("input still taken")?;
        assert_eq!(budget.available(), CHANNEL_BUFFER as usize);
        output.recv().await;
        let error = output.recv().await.context("no error")?;
        assert!(
            matches!(&error, Message::Binary(data) if data.starts_with(b"\x01error forwarding port")),
            "{error:?}"
        );
        forward.await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn tunnels_end_with_clients_which_do_not_read() -> Result<()> {
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (socket, mut client) = tokio::join!(
            WebSocketStream::from_raw_socket(web::ClientIo::new(server), Role::Server, None),
            WebSocketStream::from_raw_socket(client, Role::Client, None),
        );
        let (mut spdy, connection) = tokio::io::duplex(TUNNEL_BUFFER);
        let tunnel = tokio::spawn(PortForward::tunnel(socket, connection));

        // The SPDY connection sends more than the client reads, then ends.
        let output = vec![0; 8 * TUNNEL_BUFFER];
        assert!(
            time::timeout(Duration::from_secs(1), spdy.write_all(&output))
                .await
                .is_err()
        );
        drop(spdy);

        // The client keeps sending, which the tunnel cannot write anymore.
        let send = tokio::spawn(async move {
            while client
                .send(Message::Binary(Bytes::from_static(b"data")))
                .await
                .is_ok()
            {
                time::sleep(Duration::from_millis(100)).await;
            }
        });
        time::timeout(Duration::from_secs(30), tunnel)
            .await
            .context("the tunnel hangs")??;
        send.abort();
        Ok(())
    }

    #[tokio::test]
    async fn failures_get_reported_on_the_error_stream() -> Result<()> {
        let (_socket, port) = closed_port()?;
        let mut client = serve()?;
        request(&mut client, port).await?;

        let mut error = String::new();
        let mut reset = false;
        let mut closed = false;
        while !(reset && closed) {
            match client.frame().await?.context("connection ended")? {
                Frame::RstStream {
                    stream_id: 3,
                    status: _,
                } => reset = true,
                Frame::Data {
                    stream_id: 1,
                    fin,
                    data,
                } => {
                    error.push_str(&String::from_utf8_lossy(&data));
                    closed = fin;
                }
                frame => panic!("unexpected frame {frame:?}"),
            }
        }
        assert!(
            error.starts_with(&format!(
                "error forwarding port {port}: failed to connect to localhost:{port}"
            )),
            "{error}"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_requests_time_out() -> Result<()> {
        let mut client = serve()?;
        client.open(1, headers(STREAM_TYPE_ERROR, 80)).await?;
        client.frame().await?;

        assert_eq!(
            client.frame().await?,
            Some(Frame::Data {
                stream_id: 1,
                fin: false,
                data: Bytes::from_static(b"request 0: timed out waiting for streams")
            })
        );
        assert_eq!(
            client.frame().await?,
            Some(Frame::Data {
                stream_id: 1,
                fin: true,
                data: Bytes::new()
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_streams_get_rejected() -> Result<()> {
        let mut client = serve()?;
        for (stream_id, headers) in [
            (
                1,
                Headers::from([("streamType", "data"), ("requestID", "0")]),
            ),
            (3, [("streamType", "data"), ("port", "0")].into()),
            (5, [("streamType", "data"), ("port", "http")].into()),
            (7, [("port", "80")].into()),
            (9, [("streamType", "resize"), ("port", "80")].into()),
        ] {
            client.open(stream_id, headers).await?;
            assert!(
                matches!(client.frame().await?, Some(Frame::RstStream { stream_id: id, .. }) if id == stream_id)
            );
        }
        Ok(())
    }

    #[test]
    fn websocket_protocols_get_negotiated() {
        let negotiate =
            |requested: &[&str]| WebSocketProtocol::negotiate(requested.iter().copied());
        assert_eq!(
            negotiate(&[]),
            Some((WebSocketProtocol::Channels { base64: false }, None))
        );
        assert_eq!(
            negotiate(&["v4.base64.channel.k8s.io", "v4.channel.k8s.io"]),
            Some((
                WebSocketProtocol::Channels { base64: true },
                Some(BASE64_CHANNEL_PROTOCOL)
            ))
        );
        // The tunnel wins, wherever the client lists it.
        assert_eq!(
            negotiate(&["v4.channel.k8s.io", "SPDY/3.1+portforward.k8s.io"]),
            Some((WebSocketProtocol::SpdyTunnel, Some(SPDY_TUNNEL_PROTOCOL)))
        );
        assert_eq!(negotiate(&["v5.channel.k8s.io", "channel.k8s.io"]), None);
    }

    #[test]
    fn query_ports_get_parsed() -> Result<()> {
        assert_eq!(PortForward::query_ports(Some("port=80"))?, [80]);
        assert_eq!(
            PortForward::query_ports(Some("other=1&port=80&port=81%2C82,83"))?,
            [80, 81, 82, 83]
        );
        for query in [
            None,
            Some(""),
            Some("ports=80"),
            Some("port="),
            Some("port=0"),
            Some("port=http"),
            Some("port=65536"),
            Some("port=80,"),
        ] {
            assert!(PortForward::query_ports(query).is_err(), "{query:?}");
        }
        let too_many = vec!["port=1"; MAX_CHANNEL_PORTS + 1].join("&");
        assert!(PortForward::query_ports(Some(&too_many)).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn input_gets_taken_while_connecting() -> Result<()> {
        let (input_tx, mut input) = mpsc::channel(1);
        let (connected_tx, connected) = oneshot::channel();

        // More messages than the queue holds arrive before the connection.
        let send = tokio::spawn(async move {
            for i in 0..10u8 {
                input_tx.send(Bytes::from(vec![i]).into()).await?;
            }
            drop(input_tx);
            connected_tx
                .send(())
                .map_err(|()| anyhow::anyhow!("connect gone"))
        });
        let (connection, early) = PortForward::drain_until(
            async { connected.await.context("connected").map(|()| 42) },
            &Budget::new(CONNECT_BUDGET),
            &mut input,
        )
        .await?;
        send.await??;
        assert_eq!(connection, 42);
        // Most of it got taken, the rest is still queued for the copy.
        let mut received = early.data.to_vec();
        received.extend(early.last.iter().flat_map(|data| data.iter()));
        assert!(received.len() >= 9, "{early:?}");
        if !early.ended {
            while let Some(data) = input.recv().await {
                received.extend_from_slice(&data);
            }
        }
        assert_eq!(received, (0..10u8).collect::<Vec<_>>());
        Ok(())
    }

    #[tokio::test]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a wrapping byte pattern"
    )]
    async fn small_input_while_connecting_gets_collected() -> Result<()> {
        let connect_budget = Budget::new(CONNECT_BUDGET);
        let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
        let (connected_tx, connected) = oneshot::channel();
        let send = tokio::spawn({
            let budget = Budget::new(1000);
            async move {
                for i in 0..1000 {
                    let data = budget.hold(Bytes::from(vec![i as u8])).await;
                    input_tx.send(data).await?;
                }
                // Connected once all of it moved into the connect budget.
                while budget.available() != 1000 {
                    time::sleep(Duration::from_millis(10)).await;
                }
                connected_tx
                    .send(())
                    .map_err(|()| anyhow::anyhow!("connect gone"))
            }
        });
        let ((), early) = PortForward::drain_until(
            async { connected.await.context("connected") },
            &connect_budget,
            &mut input,
        )
        .await?;
        time::timeout(Duration::from_secs(5), send).await???;
        assert_eq!(
            &early.data[..],
            (0..1000).map(|i| i as u8).collect::<Vec<_>>()
        );
        assert!(early.last.is_none());
        // The buffer counts as a whole.
        assert!(connect_budget.available() <= CONNECT_BUDGET as usize - 1000);
        drop(early);
        assert_eq!(connect_budget.available(), CONNECT_BUDGET as usize);
        Ok(())
    }

    #[tokio::test]
    async fn written_input_gives_its_budget_back() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let connection = TcpStream::connect(listener.local_addr()?).await?;
        let (mut pod, _) = listener.accept().await?;
        let (connect_budget, client_budget) = (Budget::new(10), Budget::new(10));
        let mut data = HeldBuf::default();
        data.push(connect_budget.hold(Bytes::from_static(b"ab")).await);
        let early = Early {
            data,
            last: Some(client_budget.hold(Bytes::from_static(b"c")).await),
            ended: false,
        };

        // The client keeps its input open.
        let (_input_tx, mut input) = mpsc::channel(1);
        let (_, mut to_container) = connection.into_split();
        let write = PortForward::write_input(early, &mut input, &mut to_container);
        let given_back = async {
            let mut buf = [0; 3];
            pod.read_exact(&mut buf).await?;
            assert_eq!(&buf, b"abc");
            while connect_budget.available() != 10 || client_budget.available() != 10 {
                time::sleep(Duration::from_millis(10)).await;
            }
            anyhow::Ok(())
        };
        time::timeout(Duration::from_secs(5), async {
            select! {
                result = write => bail!("input ended: {result:?}"),
                result = given_back => result,
            }
        })
        .await
        .context("budget still held")?
    }

    #[tokio::test(start_paused = true)]
    async fn reset_streams_leave_pending_requests() -> Result<()> {
        let mut client = serve()?;
        // Requests which wait for their second stream, as many as fit.
        let headers = |typ, request_id: &str| {
            Headers::from([
                ("streamType", typ),
                ("port", "80"),
                ("requestID", request_id),
            ])
        };
        let mut stream_id = 1;
        for i in 0..MAX_STREAMS {
            let typ = [STREAM_TYPE_ERROR, STREAM_TYPE_DATA][i % 2];
            client.open(stream_id, headers(typ, &i.to_string())).await?;
            stream_id += 2;
        }
        for stream_id in (1..stream_id).step_by(2) {
            client
                .send(Frame::RstStream {
                    stream_id,
                    status: 5,
                })
                .await?;
        }

        // The next stream gets room right away, not with the deadline of
        // the requests. It may come before the resets got handled.
        let opened = async {
            loop {
                client
                    .open(stream_id, headers(STREAM_TYPE_ERROR, "next"))
                    .await?;
                let frame = client
                    .frame_until(|frame| {
                        matches!(
                            frame,
                            Frame::SynReply { stream_id: id, .. }
                                | Frame::RstStream { stream_id: id, .. } if *id == stream_id
                        )
                    })
                    .await?;
                if matches!(frame, Frame::SynReply { .. }) {
                    return anyhow::Ok(());
                }
                stream_id += 2;
                time::sleep(Duration::from_millis(10)).await;
            }
        };
        time::timeout(Duration::from_secs(5), opened)
            .await
            .context("reset streams still pending")??;
        Ok(())
    }

    #[tokio::test]
    async fn pending_requests_let_go_of_reset_streams() -> Result<()> {
        let (mut client, server) = Client::pair()?;
        let mut connection = Connection::serve(server, None, None, PortForward::accept);
        let namespace = Namespace::new(NETNS.into());
        let mut pending = Pending::new();
        // Requests a and c, whose only streams the client resets, and b.
        for (stream_id, typ, request_id) in [
            (1, STREAM_TYPE_ERROR, "a"),
            (3, STREAM_TYPE_ERROR, "b"),
            (5, STREAM_TYPE_DATA, "c"),
        ] {
            client
                .open(
                    stream_id,
                    [
                        ("streamType", typ),
                        ("port", "80"),
                        ("requestID", request_id),
                    ]
                    .into(),
                )
                .await?;
            let stream = connection.accept().await.context("stream")?;
            PortForward::add(&mut pending, stream, &namespace);
        }
        assert_eq!(pending.resets.len(), 3);
        for stream_id in [1, 5] {
            client
                .send(Frame::RstStream {
                    stream_id,
                    status: 5,
                })
                .await?;
        }
        for _ in 0..2 {
            let reset = pending.resets.next().await.context("watch")?;
            pending.remove_reset(&reset.context("reset")?);
        }
        assert_eq!(pending.requests.len(), 1);
        assert!(pending.requests.contains_key("b"));
        assert_eq!(pending.deadlines.len(), 1);

        // A request which got both streams stops watching.
        client
            .open(
                7,
                [("streamType", "data"), ("port", "80"), ("requestID", "b")].into(),
            )
            .await?;
        let stream = connection.accept().await.context("stream")?;
        PortForward::add(&mut pending, stream, &namespace);
        assert!(pending.requests.is_empty());
        assert_eq!(pending.resets.next().await, Some(None));
        assert!(pending.resets.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn channel_input_is_bounded_by_bytes() -> Result<()> {
        let budget = Budget::new(CHANNEL_BUFFER);
        let idle = CancellationToken::new();
        let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
        let inputs = [input_tx];
        let message = |len| {
            let mut message = BytesMut::with_capacity(1 + len);
            message.put_u8(0);
            message.put_bytes(1, len);
            message.freeze()
        };

        // Fewer messages than the queue holds, but all of the budget.
        let len = CHANNEL_BUFFER as usize / 4;
        for _ in 0..4 {
            PortForward::dispatch(&inputs, &budget, false, message(len), &idle).await;
        }
        assert_eq!(budget.available(), 0);
        let next = PortForward::dispatch(&inputs, &budget, false, message(1), &idle);
        let mut next = pin!(next);
        assert!(
            time::timeout(Duration::from_millis(100), &mut next)
                .await
                .is_err()
        );

        // A message which got written to the pod makes room.
        assert_eq!(input.recv().await.map(|data| data.len()), Some(len));
        time::timeout(Duration::from_secs(1), next).await?;
        Ok(())
    }

    #[tokio::test]
    async fn input_while_connecting_leaves_the_budget() -> Result<()> {
        // Connections to a port which does not answer take what the client
        // sends, which must not stop the other connections of the client.
        let budget = Budget::new(CHANNEL_BUFFER);
        let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
        let connecting = tokio::spawn(async move {
            PortForward::drain_until(
                future::pending::<Result<()>>(),
                &Budget::new(CONNECT_BUDGET),
                &mut input,
            )
            .await
        });
        for _ in 0..CHANNEL_QUEUE {
            input_tx
                .send(budget.hold(Bytes::from(vec![0; READ_BUFFER])).await)
                .await?;
        }
        time::timeout(Duration::from_secs(5), async {
            while budget.available() != CHANNEL_BUFFER as usize {
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("budget not given back")?;
        connecting.abort();
        Ok(())
    }

    #[tokio::test]
    async fn connecting_streams_stop_taking_data_without_connect_budget() -> Result<()> {
        // A port which never answers, whose client sends more than the
        // connect budget holds.
        let budget = Budget::new(CHANNEL_BUFFER);
        let connect_budget = Budget::new(4 * u32::try_from(READ_BUFFER)?);
        let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
        let connecting = tokio::spawn({
            let connect_budget = connect_budget.clone();
            async move {
                PortForward::drain_until(
                    future::pending::<Result<()>>(),
                    &connect_budget,
                    &mut input,
                )
                .await
            }
        });
        for _ in 0..8 {
            input_tx
                .send(budget.hold(Bytes::from(vec![0; READ_BUFFER])).await)
                .await?;
        }

        // The connect budget took what it holds, the rest stays in the byte
        // budget of the client, one message of it taken already.
        time::timeout(Duration::from_secs(5), async {
            while connect_budget.available() != 0
                || budget.available() != CHANNEL_BUFFER as usize - 4 * READ_BUFFER
            {
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("budgets not taken")?;
        time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            budget.available(),
            CHANNEL_BUFFER as usize - 4 * READ_BUFFER
        );

        // A connection which ends gives its share back.
        connecting.abort();
        let _ = connecting.await;
        assert_eq!(connect_budget.available(), 4 * READ_BUFFER);
        Ok(())
    }

    #[tokio::test]
    async fn parallel_connections_all_get_established() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let namespace = Namespace::with_linger(NETNS.into(), Duration::from_secs(60));
        let accept = tokio::spawn(async move {
            let mut connections = vec![];
            for _ in 0..64 {
                connections.push(listener.accept().await?);
            }
            anyhow::Ok(connections)
        });
        let mut connects = JoinSet::new();
        for i in 0..64u8 {
            let namespace = namespace.clone();
            connects.spawn(async move {
                let (input_tx, mut input) = mpsc::channel::<Held>(1);
                input_tx.send(Bytes::from(vec![i]).into()).await?;
                let (_connection, early) =
                    PortForward::establish(&namespace, port, &mut input).await?;
                anyhow::Ok(early.data.len() + early.last.map_or(0, |data| data.len()))
            });
        }
        while let Some(result) = connects.join_next().await {
            result??;
        }
        assert_eq!(
            time::timeout(Duration::from_secs(5), accept).await???.len(),
            64
        );
        assert_eq!(
            namespace.connect_budget.available(),
            CONNECT_BUDGET as usize
        );
        Ok(())
    }

    #[tokio::test]
    async fn too_much_input_while_connecting_fails() -> Result<()> {
        let (input_tx, mut input) = mpsc::channel(4);
        tokio::spawn(async move {
            while input_tx
                .send(Bytes::from(vec![0; READ_BUFFER]).into())
                .await
                .is_ok()
            {}
        });
        let err = PortForward::drain_until(
            future::pending::<Result<()>>(),
            &Budget::new(CONNECT_BUDGET),
            &mut input,
        )
        .await
        .expect_err("overflow");
        assert!(
            err.to_string()
                .contains("before the connection got established"),
            "{err:#}"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn removed_requests_free_their_deadline() {
        let mut pending = Pending::new();
        let (a, b): (Arc<str>, Arc<str>) = ("a".into(), "b".into());
        pending.get_or_insert(&a);
        pending.get_or_insert(&a);
        pending.get_or_insert(&b);
        assert_eq!(pending.deadlines.len(), 2);
        assert!(pending.remove("a").is_some());
        assert!(pending.remove("a").is_none());
        assert_eq!(pending.deadlines.len(), 1);

        let expired = time::timeout(Duration::from_secs(60), pending.deadlines.next())
            .await
            .expect("expired")
            .expect("deadline");
        assert_eq!(expired.into_inner(), b);
        assert!(pending.deadlines.is_empty());
    }

    #[tokio::test]
    async fn namespace_threads_serve_close_connections() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let namespace = Namespace::with_linger(NETNS.into(), Duration::from_secs(60));
        for _ in 0..3 {
            let _connection = PortForward::connect(&namespace, port).await?;
            listener.accept().await?;
        }
        assert_eq!(namespace.threads.load(Ordering::Relaxed), 1);

        // The thread ends without connections, the next one starts another.
        let namespace = Namespace::with_linger(NETNS.into(), Duration::from_millis(10));
        let _connection = PortForward::connect(&namespace, port).await?;
        time::timeout(Duration::from_secs(5), async {
            while namespace.ended.load(Ordering::Relaxed) == 0 {
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("the thread lingers")?;
        let _connection = PortForward::connect(&namespace, port).await?;
        assert_eq!(namespace.threads.load(Ordering::Relaxed), 2);
        Ok(())
    }

    #[tokio::test]
    async fn namespace_threads_end_with_their_session() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let namespace = Namespace::with_linger(NETNS.into(), Duration::from_secs(60));
        let _connection = PortForward::connect(&namespace, listener.local_addr()?.port()).await?;
        let ended = namespace.ended.clone();
        drop(namespace);
        time::timeout(Duration::from_secs(5), async {
            while ended.load(Ordering::Relaxed) == 0 {
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("the thread lingers")?;
        Ok(())
    }

    #[tokio::test]
    async fn every_connection_gets_its_namespace_error() -> Result<()> {
        let namespace = Namespace::new("/nonexistent".into());
        let connects = (0..10).map(|_| PortForward::connect(&namespace, 80));
        for result in futures::future::join_all(connects).await {
            let err = result.expect_err("no namespace");
            assert!(
                err.chain().any(|e| e.downcast_ref::<io::Error>().is_some()),
                "{err:#}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn namespace_failures_are_not_kept() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("netns");
        let namespace = Namespace::new(path.clone());
        let err = PortForward::connect(&namespace, 80)
            .await
            .expect_err("no namespace");
        // The error keeps its chain.
        assert!(
            err.chain().any(|e| e.downcast_ref::<io::Error>().is_some()),
            "{err:#}"
        );
        assert!(
            format!("{err:#}").contains(&format!("open network namespace {}", path.display())),
            "{err:#}"
        );

        // Once the namespace exists, the next connection works.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        std::os::unix::fs::symlink(NETNS, &path)?;
        let _connection = PortForward::connect(&namespace, listener.local_addr()?.port()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn requests_without_ids_pair_by_stream_id() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port().to_string();
        let mut client = serve()?;
        // The data stream of the request of error stream 1 is stream 3.
        client
            .open(1, [("streamType", "error"), ("port", &port)].into())
            .await?;
        client
            .open(3, [("streamType", "data"), ("port", &port)].into())
            .await?;
        time::timeout(Duration::from_secs(5), listener.accept()).await??;
        Ok(())
    }
}
