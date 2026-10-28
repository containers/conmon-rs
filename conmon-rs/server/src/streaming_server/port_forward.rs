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

use crate::streaming_server::{
    budget::{Budget, Held, HeldBuf},
    spdy::{PING_INTERVAL, StreamReceiver},
};
use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use std::{
    future::{self, Future},
    io,
    path::PathBuf,
    pin::pin,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpSocket, TcpStream, tcp::OwnedWriteHalf},
    select,
    sync::mpsc,
    time::{self, Duration},
};

mod channels;
mod namespace;
mod requests;
mod tunnel;

use namespace::Namespace;
pub use tunnel::WebSocketProtocol;

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

/// A port forward session of a pod.
#[derive(Debug)]
pub struct PortForward {
    netns_path: PathBuf,
    idle_timeout: Option<Duration>,
    ping_interval: Option<Duration>,
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
pub mod fuzzing;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod websocket_tests;
