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
    idle::Activity,
    spdy::{Connection, Headers, PING_INTERVAL, Stream, StreamReceiver, StreamSender},
};
use anyhow::{Context, Result, bail, ensure};
use axum::extract::ws::{CloseFrame, Message, WebSocket, close_code};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use bytes::{BufMut, Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use nix::{
    sched::{CloneFlags, setns},
    sys::socket::{AddressFamily, SockFlag, SockType, socket},
};
use percent_encoding::percent_decode_str;
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream as StdTcpStream},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    pin::pin,
    sync::{Arc, Mutex, MutexGuard, mpsc as std_mpsc},
    thread,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    net::{TcpSocket, TcpStream, tcp::OwnedWriteHalf},
    select,
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{self, Duration},
};
use tokio_util::{
    sync::CancellationToken,
    time::{DelayQueue, delay_queue::Key},
};
use tracing::{debug, trace, warn};

/// The protocol the client has to ask for in the upgrade request.
pub const PROTOCOL: &str = "portforward.k8s.io";

const PORT_HEADER: &str = "port";
const REQUEST_ID_HEADER: &str = "requestID";
const STREAM_TYPE_HEADER: &str = "streamType";
const STREAM_TYPE_DATA: &str = "data";
const STREAM_TYPE_ERROR: &str = "error";

/// How long the second stream of a request may take, the default of the
/// kubelet and CRI-O.
const STREAM_CREATION_TIMEOUT: Duration = Duration::from_secs(30);

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
/// the pod. Its data gets taken meanwhile, so that it does not hold up the
/// other connections of the client.
const CONNECT_BUFFER: usize = 1024 * 1024;

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

/// How many messages for the client wait for the WebSocket.
const OUTPUT_QUEUE: usize = 16;

/// The size of the buffers between the WebSocket and the tunneled SPDY
/// connection.
const TUNNEL_BUFFER: usize = 64 * 1024;

/// How long the client gets to answer the close of the server.
const WEBSOCKET_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// The data a client sends for a forwarded connection.
trait Input {
    /// The next data, `None` once the client is done.
    async fn recv(&mut self) -> Option<Bytes>;
}

impl Input for StreamReceiver {
    async fn recv(&mut self) -> Option<Bytes> {
        StreamReceiver::recv(self).await
    }
}

impl Input for mpsc::Receiver<Bytes> {
    async fn recv(&mut self) -> Option<Bytes> {
        mpsc::Receiver::recv(self).await
    }
}

/// What a client sent while its connection got established.
#[derive(Debug, Default)]
struct Early {
    data: VecDeque<Bytes>,

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
        let message = if self.base64 {
            let mut text = String::with_capacity(1 + data.len().div_ceil(3) * 4);
            text.push(char::from(b'0' + channel));
            BASE64.encode_string(data, &mut text);
            Message::Text(text.into())
        } else {
            let mut frame = BytesMut::with_capacity(1 + data.len());
            frame.put_u8(channel);
            frame.put_slice(data);
            Message::Binary(frame.freeze())
        };
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
    data: Option<Stream>,
    error: Option<Stream>,

    /// The deadline of the request in [`Pending::deadlines`].
    deadline: Key,
}

/// The requests waiting for their second stream, with their deadlines. Both
/// are bounded by the streams of a connection.
struct Pending {
    requests: HashMap<Arc<str>, PendingRequest>,
    deadlines: DelayQueue<Arc<str>>,
}

impl Pending {
    fn new() -> Self {
        Self {
            requests: HashMap::new(),
            deadlines: DelayQueue::new(),
        }
    }

    /// The request, created with its deadline if it is new.
    fn get_or_insert(&mut self, request_id: &Arc<str>) -> &mut PendingRequest {
        let Self {
            requests,
            deadlines,
        } = self;
        requests
            .entry(request_id.clone())
            .or_insert_with(|| PendingRequest {
                data: None,
                error: None,
                deadline: deadlines.insert(request_id.clone(), STREAM_CREATION_TIMEOUT),
            })
    }

    /// Remove a request which got both streams, with its deadline.
    fn complete(&mut self, request_id: &str) -> Option<PendingRequest> {
        let request = self.requests.remove(request_id)?;
        self.deadlines.remove(&request.deadline);
        Some(request)
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
        self.requests.lock().unwrap_or_else(|e| e.into_inner())
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
                self.serve_channels(socket, ports, base64).await
            }
        }
        debug!("Port forward WebSocket ended");
    }

    /// Carry the bytes of the SPDY connection in binary messages, until
    /// either side ends.
    async fn tunnel(socket: WebSocket, connection: DuplexStream) {
        let (mut sink, mut messages) = socket.split();
        let (mut from_server, mut to_server) = tokio::io::split(connection);

        let mut to_client: JoinHandle<()> = tokio::spawn(async move {
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
                            return;
                        }
                    }
                }
            }
            let _ = sink
                .send(Message::Close(Some(CloseFrame {
                    code: close_code::NORMAL,
                    reason: "".into(),
                })))
                .await;
        });

        let from_client = async {
            while let Some(message) = messages.next().await {
                match message {
                    Ok(Message::Binary(data)) => {
                        if to_server.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Text(_)) => {
                        debug!("Ending port forward tunnel, the client sent a text message");
                        break;
                    }
                    Ok(Message::Ping(_) | Message::Pong(_)) => {}
                    Ok(Message::Close(_)) | Err(_) => break,
                }
            }
            // Ends the SPDY connection, which ends the output as well.
            let _ = to_server.shutdown().await;
        };

        let mut from_client = pin!(from_client);
        let client_done = select! {
            _ = &mut from_client => true,
            // The SPDY connection ended, the client answers the close.
            _ = &mut to_client => false,
        };
        if client_done {
            Self::join_or_abort(to_client, WEBSOCKET_CLOSE_TIMEOUT).await;
        } else {
            let _ = time::timeout(WEBSOCKET_CLOSE_TIMEOUT, from_client).await;
        }
    }

    /// Serve the channel protocol: forward a connection per port until all
    /// of them ended, the client went away or the WebSocket was idle.
    async fn serve_channels(self, socket: WebSocket, ports: Vec<u16>, base64: bool) {
        let (mut sink, mut messages) = socket.split();
        let (output_tx, mut output_rx) = mpsc::channel(OUTPUT_QUEUE);
        let activity = Activity::new();

        // On its own, as a port which does not take its input stops reading
        // the WebSocket.
        let idle = CancellationToken::new();
        // Ends the watch however the session ends.
        let _idle_guard = idle.clone().drop_guard();
        tokio::spawn({
            let (activity, idle, timeout) = (activity.clone(), idle.clone(), self.idle_timeout);
            async move {
                select! {
                    _ = activity.idle(timeout) => {
                        debug!("Closing idle port forward WebSocket");
                        idle.cancel();
                    }
                    _ = idle.cancelled() => {}
                }
            }
        });

        let writer_activity = activity.clone();
        let writer = tokio::spawn(async move {
            while let Some(message) = output_rx.recv().await {
                if sink.send(message).await.is_err() {
                    return;
                }
                writer_activity.touch();
            }
            let _ = sink
                .send(Message::Close(Some(CloseFrame {
                    code: close_code::NORMAL,
                    reason: "".into(),
                })))
                .await;
        });

        let namespace = Namespace::new(self.netns_path);
        let mut inputs = Vec::with_capacity(ports.len());
        let mut forwards = JoinSet::new();
        for (i, port) in ports.into_iter().enumerate() {
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

        loop {
            select! {
                message = messages.next() => {
                    activity.touch();
                    match message {
                        Some(Ok(Message::Binary(data))) => {
                            Self::dispatch(&inputs, base64, data, &idle).await
                        }
                        Some(Ok(Message::Text(text))) => {
                            Self::dispatch(&inputs, base64, Bytes::from(text), &idle).await
                        }
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    }
                }
                done = forwards.join_next() => if done.is_none() {
                    break;
                },
                _ = idle.cancelled() => break,
            }
        }

        // The ports still forwarded lose their client.
        forwards.abort_all();
        while forwards.join_next().await.is_some() {}
        Self::join_or_abort(writer, WEBSOCKET_CLOSE_TIMEOUT).await;
    }

    /// Wait for a task, abort it after the timeout. Dropping its handle would
    /// leave it running, a writer to a client which does not read would hold
    /// the connection.
    async fn join_or_abort(mut task: JoinHandle<()>, timeout: Duration) {
        if time::timeout(timeout, &mut task).await.is_err() {
            task.abort();
        }
    }

    /// Hand a message of the client to the connection of its channel. Only
    /// the data channels, the even ones, take input. Waits while a connected
    /// port does not take its input, which stops reading the WebSocket like
    /// the reference implementation does, the protocol has no flow control.
    /// Connecting ports take it, see [`Self::drain_until`], and the idle
    /// timeout still ends the wait.
    async fn dispatch(
        inputs: &[mpsc::Sender<Bytes>],
        base64: bool,
        message: Bytes,
        idle: &CancellationToken,
    ) {
        let Some(&channel) = message.first() else {
            return;
        };
        let channel = if base64 {
            channel.wrapping_sub(b'0')
        } else {
            channel
        };
        let Some(input) = inputs
            .get(usize::from(channel / 2))
            .filter(|_| channel % 2 == 0)
        else {
            trace!("Dropping port forward message for channel {channel}");
            return;
        };

        let data = if base64 {
            match BASE64.decode(&message[1..]) {
                Ok(data) => Bytes::from(data),
                Err(e) => {
                    debug!("Dropping port forward message with invalid base64: {e}");
                    return;
                }
            }
        } else {
            message.slice(1..)
        };
        if !data.is_empty() {
            select! {
                // A connection which ended does not take input anymore.
                _ = input.send(data) => {}
                _ = idle.cancelled() => {}
            }
        }
    }

    /// Forward the connection of a port, the error channel reports a failure.
    async fn forward_channel(
        namespace: Arc<Namespace>,
        port: u16,
        mut input: mpsc::Receiver<Bytes>,
        output: ChannelOutput,
    ) {
        debug!("Forwarding port {port} over WebSocket");
        let result = match Self::drain_until(Self::connect(&namespace, port), &mut input).await {
            Ok((connection, early)) => {
                Self::copy_channel(early, &mut input, &output, connection).await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            let message = format!("error forwarding port {port}: {e:#}");
            warn!("Port forward over WebSocket: {message}");
            let _ = output.send(output.error, message.as_bytes()).await;
        }
    }

    /// Copy between the channels of a port and its connection in the pod,
    /// until the pod closed the connection or the client went away. The
    /// protocol has no way to close a channel, the input after the pod closed
    /// the connection does not matter.
    async fn copy_channel(
        early: Early,
        input: &mut mpsc::Receiver<Bytes>,
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
            Some(typ) => bail!("invalid stream type {typ:?}"),
            None => bail!("{STREAM_TYPE_HEADER:?} header is required"),
        }
    }

    fn port(headers: &Headers) -> Result<u16> {
        let port = headers
            .get(PORT_HEADER)
            .with_context(|| format!("{PORT_HEADER:?} header is required"))?;
        let port: u16 = port
            .parse()
            .with_context(|| format!("unable to parse {port:?} as a port"))?;
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
        let request = pending.get_or_insert(&request_id);
        let slot = if Self::is_error_stream(&stream) {
            &mut request.error
        } else {
            &mut request.data
        };
        if slot.is_some() {
            // Dropping it resets it.
            warn!(
                "Rejecting stream {} of port forward request {request_id}, the request has one of its type already",
                stream.id()
            );
            return;
        }
        *slot = Some(stream);

        if request.data.is_none() || request.error.is_none() {
            return;
        }
        if let Some(PendingRequest {
            data: Some(data),
            error: Some(error),
            ..
        }) = pending.complete(&request_id)
        {
            tokio::spawn(Self::forward(request_id, data, error, namespace.clone()));
        }
    }

    /// Fail a request whose second stream did not arrive in time.
    fn expire(pending: &mut Pending, request_id: Arc<str>) {
        let Some(mut request) = pending.requests.remove(&request_id) else {
            return;
        };
        warn!("Port forward request {request_id} timed out waiting for streams");
        if let Some(mut error) = request.error.take() {
            let message = format!("request {request_id}: timed out waiting for streams");
            tokio::spawn(async move {
                let _ = error.send(message.into()).await;
                let _ = error.close().await;
            });
        }
    }

    /// Forward a single connection, the error stream reports a failure.
    async fn forward(
        request_id: Arc<str>,
        data: Stream,
        mut error: Stream,
        namespace: Arc<Namespace>,
    ) {
        let port = Self::port(data.headers()).unwrap_or_default();
        debug!("Forwarding port {port} for request {request_id}");

        let (mut from_client, to_client) = data.split();
        let result = match Self::drain_until(Self::connect(&namespace, port), &mut from_client)
            .await
        {
            Ok((connection, early)) => Self::copy(early, from_client, to_client, connection).await,
            Err(e) => {
                // Dropping the sending side resets the data stream.
                drop(to_client);
                Err(e)
            }
        };
        match result {
            Ok(()) => debug!("Done forwarding port {port} for request {request_id}"),
            Err(e) => {
                let message = format!("error forwarding port {port}: {e:#}");
                warn!("Port forward request {request_id}: {message}");
                if let Err(e) = error.send(message.into()).await {
                    debug!("Unable to report port forward error: {e:#}");
                }
            }
        }
        if let Err(e) = error.close().await {
            debug!("Unable to close port forward error stream: {e:#}");
        }
    }

    /// Wait for the connection to the pod while taking what the client sends
    /// meanwhile, which may take until the connect timeout. A client which
    /// sends more than [`CONNECT_BUFFER`] fails the forwarding.
    async fn drain_until<T>(
        connect: impl Future<Output = Result<T>>,
        input: &mut impl Input,
    ) -> Result<(T, Early)> {
        let mut connect = pin!(connect);
        let mut early = Early::default();
        let mut len = 0;
        loop {
            select! {
                result = &mut connect => return Ok((result?, early)),
                data = input.recv(), if !early.ended => match data {
                    Some(data) => {
                        len += data.len();
                        ensure!(
                            len <= CONNECT_BUFFER,
                            "the client sent more than {CONNECT_BUFFER} bytes before the connection got established"
                        );
                        early.data.push_back(data);
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
                    _ = to_client.reset_by_peer() => return Ok(()),
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
    /// starting with what it sent while the connection got established.
    async fn write_input(
        early: Early,
        input: &mut impl Input,
        to_container: &mut OwnedWriteHalf,
    ) -> Result<()> {
        for data in early.data {
            to_container
                .write_all(&data)
                .await
                .context("write to the pod")?;
        }
        if !early.ended {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spdy::testing::{Client, Frame};
    use std::future;
    use tokio::net::TcpListener;

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
    async fn failures_get_reported_on_the_error_stream() -> Result<()> {
        // A port nothing listens on anymore.
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await?
            .local_addr()?
            .port();
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
                input_tx.send(Bytes::from(vec![i])).await?;
            }
            drop(input_tx);
            connected_tx
                .send(())
                .map_err(|_| anyhow::anyhow!("connect gone"))
        });
        let (connection, early) = PortForward::drain_until(
            async { connected.await.context("connected").map(|()| 42) },
            &mut input,
        )
        .await?;
        send.await??;
        assert_eq!(connection, 42);
        // Most of it got taken, the rest is still queued for the copy.
        assert!(early.data.len() >= 9, "{early:?}");
        let mut received = Vec::from(early.data);
        if !early.ended {
            while let Some(data) = input.recv().await {
                received.push(data);
            }
        }
        assert_eq!(
            received,
            (0..10u8).map(|i| Bytes::from(vec![i])).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[tokio::test]
    async fn too_much_input_while_connecting_fails() -> Result<()> {
        let (input_tx, mut input) = mpsc::channel(4);
        tokio::spawn(async move {
            while input_tx
                .send(Bytes::from(vec![0; READ_BUFFER]))
                .await
                .is_ok()
            {}
        });
        let err = PortForward::drain_until(future::pending::<Result<()>>(), &mut input)
            .await
            .expect_err("overflow");
        assert!(
            err.to_string()
                .contains("before the connection got established"),
            "{err:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn hanging_tasks_get_aborted() -> Result<()> {
        let (alive_tx, alive_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _alive = alive_tx;
            future::pending::<()>().await
        });
        PortForward::join_or_abort(task, Duration::from_millis(10)).await;
        // The task got dropped with its sender.
        assert!(
            time::timeout(Duration::from_secs(5), alive_rx)
                .await?
                .is_err()
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn completed_requests_free_their_deadline() {
        let mut pending = Pending::new();
        let (a, b): (Arc<str>, Arc<str>) = ("a".into(), "b".into());
        pending.get_or_insert(&a);
        pending.get_or_insert(&a);
        pending.get_or_insert(&b);
        assert_eq!(pending.deadlines.len(), 2);
        assert!(pending.complete("a").is_some());
        assert!(pending.complete("a").is_none());
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
        time::sleep(Duration::from_millis(200)).await;
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
