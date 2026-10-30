//! The Kubernetes remote command protocols of exec and attach sessions, as
//! k8s.io/cri-streaming serves them for CRI-O otherwise.
//!
//! Clients speak them over WebSocket, `v5.channel.k8s.io` and the versions
//! before it, or over SPDY, which the kubelet uses towards the runtime also
//! for clients which connected with a WebSocket. Each transport turns the
//! input of the client into [`ClientEvent`]s, and [`Output`] sends the output
//! of a session the way its protocol needs it.

use crate::streaming_server::{
    idle::{Activity, STALL_TIMEOUT},
    web::{self, Message, WebSocket},
};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use futures::{sink::SinkExt, stream::SplitSink};
use std::{
    future::{self, Future},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::time::{Duration, Instant};
use tracing::debug;

mod spdy;
mod status;
mod websocket;

use spdy::SpdyOutput;
pub use spdy::{ExpectedStreams, SPDY_PROTOCOLS, SpdyStreams};
pub use status::{Operation, Status};
pub use websocket::{Framing, WebSocketInput};

pub const STDIN_BYTE: u8 = 0;
pub const STDOUT_BYTE: u8 = 1;
pub const STDERR_BYTE: u8 = 2;
pub const STREAM_ERR_BYTE: u8 = 3;
pub const RESIZE_BYTE: u8 = 4;
pub const CLOSE_BYTE: u8 = 255;

/// The status of a session which succeeded. The kubelet sends it as well,
/// some clients take an empty error stream for a failure.
const SUCCESS_STATUS: &[u8] = b"{\"metadata\":{},\"status\":\"Success\"}";

/// The most resize events a SPDY client may send without completing one.
const MAX_RESIZE_BUFFER: usize = 4 * 1024;

const STREAM_TYPE_RESIZE: &str = "resize";

/// A version of the protocols, which decides how a session reports its
/// status and whether the client resizes terminals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
    V1,
    V2,
    V3,
    V4,
    V5,
}

impl Version {
    /// Whether the status is JSON, otherwise only a failure is reported as
    /// text.
    fn json_status(self) -> bool {
        self >= Self::V4
    }

    /// Whether a SPDY client with a terminal opens a stream for resize
    /// events.
    pub fn spdy_resize(self) -> bool {
        self >= Self::V3
    }
}

/// What a client sends during a session.
#[derive(Debug, PartialEq)]
pub enum ClientEvent {
    Stdin(Stdin),

    /// A resize event as JSON.
    Resize(Bytes),

    /// The client closed the connection cleanly: a WebSocket with a close,
    /// a SPDY connection which ended after the stdin of the client ended.
    Closed,

    /// The client sent a message larger than the server takes, which fails
    /// the session, see [`MESSAGE_TOO_LARGE`].
    TooLarge,
}

/// The failure of a session whose client sent a message larger than the
/// server takes.
pub const MESSAGE_TOO_LARGE: &str = "message of the client exceeds the size limit of the server";

/// The stdin of a client.
#[derive(Clone, Debug, PartialEq)]
pub enum Stdin {
    Data(Bytes),

    /// The client closed its stdin.
    Close,
}

impl Stdin {
    /// Stdin with data, none for an empty message, which carries nothing.
    pub fn data(data: Bytes) -> Option<Self> {
        (!data.is_empty()).then_some(Self::Data(data))
    }
}

/// The input of a client.
pub trait ClientInput: Send + 'static {
    /// The next event, `None` once the client went away without a clean
    /// close.
    fn next(&mut self) -> impl Future<Output = Option<ClientEvent>> + Send;

    /// End the connection once `output` closed it after
    /// [`ClientEvent::TooLarge`], see [`web::drain`].
    fn drain(self, output: Output) -> impl Future<Output = ()> + Send
    where
        Self: Sized,
    {
        drop(output);
        future::ready(())
    }
}

/// The sending half of a session. A failing send means that the client went
/// away, which ends the session. After a clean close of a WebSocket client,
/// sends fail as well, the output gets dropped then. The session still
/// forwards the stdin which the client sent before its close to the process
/// and reads the output meanwhile, so that the process does not wait for it.
/// A SPDY connection which ends after the client closed its stdin counts as
/// a clean close.
pub struct Output {
    transport: Transport,
    failed: bool,

    /// Set once the client closed the connection cleanly.
    peer_closed: Arc<AtomicBool>,

    /// Whether a WebSocket gets closed for a message of the client which
    /// was too large.
    too_large: bool,

    /// When the end of the session started, the status and the close share
    /// the [`STALL_TIMEOUT`] from then on, a client which takes nothing does
    /// not get it twice.
    ending: Option<Instant>,

    /// The idle timeout of the session, which its end waits for as well:
    /// a client which takes nothing, like one which pauses a pager, keeps
    /// the end until it took nothing for the idle timeout, at least for the
    /// [`STALL_TIMEOUT`], see [`Activity::until_stalled_or_idle`]. Without
    /// one, the end waits for the [`STALL_TIMEOUT`] only. A client which
    /// vanished without a reset while its output backs up holds the end as
    /// long as well, or until the kernel gives up resending to it, 15 to 30
    /// minutes with the default `tcp_retries2` of 15: there is no keepalive,
    /// and nothing waits for the echoes of pings. None once the session
    /// failed for a message of the client which was too large, see
    /// [`Self::close_too_large`].
    idle_timeout: Option<Duration>,
}

enum Transport {
    WebSocket {
        sender: SplitSink<WebSocket, Message>,
        framing: Framing,

        /// Touched by every message with data which got sent, not by pings
        /// and the close.
        activity: Activity,

        /// Touched whenever a part of what the server sends went out, see
        /// [`web::ClientIo::written`].
        written: Activity,
    },
    Spdy(SpdyOutput),
}

impl Output {
    /// The output of a WebSocket session, whose end waits for the
    /// `idle_timeout` of the session as well, see [`Self::idle_timeout`].
    pub fn websocket(
        sender: SplitSink<WebSocket, Message>,
        framing: Framing,
        activity: Activity,
        written: Activity,
        idle_timeout: Option<Duration>,
    ) -> Self {
        let transport = Transport::WebSocket {
            sender,
            framing,
            activity,
            written,
        };
        Self::new(transport, Arc::default(), idle_timeout)
    }

    /// The output of a SPDY session, which shares whether the client closed
    /// its stdin with its input, see [`SpdyInput`](spdy::SpdyInput), and
    /// whose end waits for the `idle_timeout` of the session as well, see
    /// [`Self::idle_timeout`].
    pub fn spdy(output: SpdyOutput, idle_timeout: Option<Duration>) -> Self {
        let peer_closed = output.peer_closed.clone();
        Self::new(Transport::Spdy(output), peer_closed, idle_timeout)
    }

    fn new(
        transport: Transport,
        peer_closed: Arc<AtomicBool>,
        idle_timeout: Option<Duration>,
    ) -> Self {
        Self {
            transport,
            failed: false,
            peer_closed,
            too_large: false,
            ending: None,
            idle_timeout,
        }
    }

    /// Close a WebSocket with the code for a message of the client which was
    /// too large, which tells the client why the session failed. The session
    /// failed, its status and its close do not wait for the idle timeout,
    /// only for the [`STALL_TIMEOUT`].
    pub fn close_too_large(&mut self) {
        self.too_large = true;
        self.idle_timeout = None;
    }

    /// Set by the input once the client closed the connection cleanly.
    pub fn peer_closed(&self) -> Arc<AtomicBool> {
        self.peer_closed.clone()
    }

    /// Tell a WebSocket client before v5 that the session started, with an
    /// empty message on the first channel it gets output on.
    pub async fn announce(&mut self, stdout: bool, stderr: bool) -> Result<()> {
        let Transport::WebSocket { framing, .. } = &self.transport else {
            return Ok(());
        };
        if framing.version == Version::V5 {
            return Ok(());
        }
        let channel = if stdout {
            STDOUT_BYTE
        } else if stderr {
            STDERR_BYTE
        } else {
            STREAM_ERR_BYTE
        };
        let message = framing.message(channel, &[]);
        self.send(message).await
    }

    /// Send output of the process, `stream_byte` selects stdout or stderr.
    pub async fn data(&mut self, stream_byte: u8, data: &Bytes) -> Result<()> {
        let result = match &mut self.transport {
            Transport::WebSocket { framing, .. } => {
                let message = framing.message(stream_byte, data);
                return self.send(message).await;
            }
            Transport::Spdy(output) => {
                let stream = if stream_byte == STDOUT_BYTE {
                    &mut output.stdout
                } else {
                    &mut output.stderr
                };
                match stream {
                    Some(stream) if !self.failed => stream.send(data.clone()).await,
                    _ => Ok(()),
                }
            }
        };
        self.sent(result)
    }

    /// Send the final status of the session. Over SPDY it waits behind the
    /// output queued before, a slow client gets it all the same, one which
    /// stopped reading for [`STALL_TIMEOUT`] does not get it, see
    /// [`Self::idle_timeout`].
    pub async fn status(&mut self, status: &Status) -> Result<()> {
        let ending = self.ending();
        let idle_timeout = self.idle_timeout;
        self.written()
            .until_stalled_or_idle(
                ending,
                STALL_TIMEOUT,
                idle_timeout,
                self.send_status(status),
            )
            .await
            .unwrap_or_else(|| Err(anyhow!("client stopped reading before the status")))
    }

    /// When the end of the session started, the first time this got
    /// called.
    fn ending(&mut self) -> Instant {
        *self.ending.get_or_insert_with(Instant::now)
    }

    /// Touched whenever a part of what the server sends went out.
    fn written(&self) -> Activity {
        match &self.transport {
            Transport::WebSocket { written, .. } => written.clone(),
            Transport::Spdy(output) => output.connection.written(),
        }
    }

    async fn send_status(&mut self, status: &Status) -> Result<()> {
        let result = match &mut self.transport {
            Transport::WebSocket { framing, .. } => {
                let Some(status) = status.encode(framing.version)? else {
                    return Ok(());
                };
                let message = framing.message(STREAM_ERR_BYTE, &status);
                return self.send(message).await;
            }
            Transport::Spdy(output) => match status.encode(output.version)? {
                Some(status) if !self.failed => output.error.send(status.into()).await,
                _ => Ok(()),
            },
        };
        self.sent(result)
    }

    /// Check that the client is still there while the session waits. A ping
    /// to a client which went away fails. A SPDY connection pings the client
    /// itself, also when it is not read while stdin waits for the process,
    /// and ends once a write fails.
    pub async fn ping(&mut self) -> Result<()> {
        match &self.transport {
            Transport::WebSocket { .. } => self.send(Message::Ping(Bytes::new())).await,
            Transport::Spdy(output) if output.connection.done().is_cancelled() => {
                self.sent(Err(anyhow!("SPDY connection closed")))
            }
            Transport::Spdy(_) => Ok(()),
        }
    }

    /// End the session: a WebSocket gets closed, the SPDY streams end and the
    /// writer flushes what is queued, shuts the connection down and ends it.
    /// A client which takes it slowly gets all of it, one which stopped
    /// reading gets the connection ended after [`STALL_TIMEOUT`], counted
    /// from the status on, see [`Self::idle_timeout`]. Whether the close
    /// went out, which the client of a WebSocket answers.
    pub async fn close(&mut self) -> bool {
        let ending = self.ending();
        let written = self.written();
        let idle_timeout = self.idle_timeout;
        let close = async {
            match &mut self.transport {
                Transport::WebSocket { .. } => {
                    if let Err(e) = self.send(web::close(self.too_large, "done")).await {
                        debug!("Unable to send close message: {e:#}");
                        return false;
                    }
                }
                Transport::Spdy(output) => {
                    for stream in [
                        output.stdin.as_mut(),
                        output.stdout.as_mut(),
                        output.stderr.as_mut(),
                    ]
                    .into_iter()
                    .flatten()
                    .chain([&mut output.error])
                    {
                        if let Err(e) = stream.close().await {
                            debug!("Unable to close stream: {e:#}");
                        }
                    }
                    output.connection.close();
                    output.connection.done().cancelled().await;
                }
            }
            true
        };
        let closed = written
            .until_stalled_or_idle(ending, STALL_TIMEOUT, idle_timeout, close)
            .await
            .unwrap_or_else(|| {
                debug!("Client stopped reading before the end of the session");
                false
            });
        if let Transport::Spdy(output) = &self.transport {
            output.connection.abort();
        }
        closed
    }

    /// Send a WebSocket message. A failure ends the session, unless the
    /// client closed the connection cleanly, see [`Output`].
    async fn send(&mut self, message: Message) -> Result<()> {
        if self.failed {
            return Ok(());
        }
        let Transport::WebSocket {
            sender, activity, ..
        } = &mut self.transport
        else {
            return Ok(());
        };
        let data = !matches!(message, Message::Ping(_) | Message::Close(_));
        let result = sender.send(message).await.map_err(anyhow::Error::from);
        if result.is_ok() && data {
            activity.touch();
        }
        self.sent(result)
    }

    /// A send failed after a clean close: of a WebSocket, or of a SPDY
    /// connection which ended after the stdin of the client ended. A stream
    /// of a connection which is still up got reset, which fails the session.
    fn closed(&self) -> bool {
        self.peer_closed.load(Ordering::Acquire)
            && match &self.transport {
                Transport::WebSocket { .. } => true,
                Transport::Spdy(output) => output.connection.done().is_cancelled(),
            }
    }

    fn sent(&mut self, result: Result<()>) -> Result<()> {
        match result {
            Ok(()) => Ok(()),
            Err(e) if self.closed() => {
                debug!("Dropping the output of the session after the close: {e:#}");
                self.failed = true;
                Ok(())
            }
            Err(e) => Err(e).context("send to client"),
        }
    }
}

/// The entry points of the fuzz targets.
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzzing;

#[cfg(test)]
mod tests;
