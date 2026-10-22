//! The Kubernetes remote command protocols of exec and attach sessions, as
//! k8s.io/cri-streaming serves them for CRI-O otherwise.
//!
//! Clients speak them over WebSocket, `v5.channel.k8s.io` and the versions
//! before it, or over SPDY, which the kubelet uses towards the runtime also
//! for clients which connected with a WebSocket. Each transport turns the
//! input of the client into [`ClientEvent`]s, and [`Output`] sends the output
//! of a session the way its protocol needs it.

use crate::{
    channel,
    idle::Activity,
    spdy::{Connection, ConnectionHandle, Headers, Stream, StreamReceiver, StreamSender},
    stream_protocol::{ClientBytes, ClientStr, STREAM_CREATION_TIMEOUT, STREAM_TYPE_HEADER},
    web::{self, Message, WebSocket},
};
use anyhow::{Context, Result, anyhow, bail};
use bytes::{Bytes, BytesMut};
use futures::{
    sink::SinkExt,
    stream::{SplitSink, SplitStream, StreamExt},
};
use serde::{Serialize, de::IgnoredAny};
use std::{
    collections::VecDeque,
    future::{self, Future},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    select,
    time::{self, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace};

pub const STDIN_BYTE: u8 = 0;
pub const STDOUT_BYTE: u8 = 1;
pub const STDERR_BYTE: u8 = 2;
pub const STREAM_ERR_BYTE: u8 = 3;
pub const RESIZE_BYTE: u8 = 4;
pub const CLOSE_BYTE: u8 = 255;

/// The status of a session which succeeded. The kubelet sends it as well,
/// some clients take an empty error stream for a failure.
const SUCCESS_STATUS: &[u8] = b"{\"metadata\":{},\"status\":\"Success\"}";

/// How long the client gets to take the end of a session, its status and the
/// close, before the server gives up on it.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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

/// The SPDY protocols, the kubelet and client-go prefer the first one.
pub const SPDY_PROTOCOLS: [(&str, Version); 4] = [
    ("v4.channel.k8s.io", Version::V4),
    ("v3.channel.k8s.io", Version::V3),
    ("v2.channel.k8s.io", Version::V2),
    ("channel.k8s.io", Version::V1),
];

/// The version of a SPDY protocol.
pub fn spdy_version(protocol: &str) -> Option<Version> {
    SPDY_PROTOCOLS
        .iter()
        .find_map(|(p, version)| (*p == protocol).then_some(*version))
}

/// How the messages of a WebSocket session look.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Framing {
    pub version: Version,

    /// Whether the payloads are base64 encoded in text messages, see
    /// [`channel`].
    pub base64: bool,
}

/// The WebSocket subprotocols, without one a client gets binary `v1`.
const WEBSOCKET_PROTOCOLS: [(&str, Framing); 5] = [
    ("v5.channel.k8s.io", Framing::V5),
    (
        "v4.channel.k8s.io",
        Framing {
            version: Version::V4,
            base64: false,
        },
    ),
    (
        "v4.base64.channel.k8s.io",
        Framing {
            version: Version::V4,
            base64: true,
        },
    ),
    (
        "channel.k8s.io",
        Framing {
            version: Version::V1,
            base64: false,
        },
    ),
    (
        "base64.channel.k8s.io",
        Framing {
            version: Version::V1,
            base64: true,
        },
    ),
];

impl Framing {
    pub const V5: Self = Self {
        version: Version::V5,
        base64: false,
    };

    /// The framing for the subprotocols the client requested, in the order
    /// of the client, with the subprotocol the server answers with.
    pub fn negotiate<'a>(
        requested: impl IntoIterator<Item = &'a str>,
    ) -> Option<(Self, Option<&'static str>)> {
        let mut requested = requested.into_iter().peekable();
        if requested.peek().is_none() {
            return Some((
                Self {
                    version: Version::V1,
                    base64: false,
                },
                None,
            ));
        }
        requested.find_map(|protocol| {
            WEBSOCKET_PROTOCOLS
                .iter()
                .find_map(|(p, framing)| (*p == protocol).then_some((*framing, Some(*p))))
        })
    }

    /// A message of the server on a channel.
    pub fn message(self, channel: u8, data: &[u8]) -> Message {
        channel::message(self.base64, channel, data)
    }

    /// What a message of the client means, none for one which needs no
    /// handling.
    pub fn event(self, message: Message) -> Option<ClientEvent> {
        let data = match message {
            Message::Binary(data) => data,
            Message::Text(text) if self.version < Version::V5 => Bytes::from(text),
            Message::Text(t) => {
                trace!("Got text message: {t:?}");
                return None;
            }
            Message::Close(c) => {
                match c {
                    Some(cf) => info!(
                        "Got websocket close with code {} and reason {:?}",
                        cf.code,
                        ClientStr(&cf.reason)
                    ),
                    None => info!("Got websocket close without close frame"),
                }
                return Some(ClientEvent::Closed);
            }
            Message::Pong(_) => {
                trace!("Got pong");
                return None;
            }
            Message::Ping(_) => {
                trace!("Got ping");
                return None;
            }
            // Raw frames only get sent, reads never return them.
            Message::Frame(_) => return None,
        };

        if self.version == Version::V5 {
            let Some(&channel) = data.first() else {
                trace!("Got unknown binary data");
                return None;
            };
            trace!("Got {} binary bytes", data.len());
            return match channel {
                STDIN_BYTE => Stdin::data(data.slice(1..)).map(ClientEvent::Stdin),
                // Only stdin can be closed, the message names it, like
                // `[CLOSE_BYTE, STDIN_BYTE]`.
                CLOSE_BYTE if data[..] == [CLOSE_BYTE, STDIN_BYTE] => {
                    debug!("Got close message");
                    Some(ClientEvent::Stdin(Stdin::Close))
                }
                CLOSE_BYTE => {
                    debug!("Ignoring close message {:?}", ClientBytes(&data[1..]));
                    None
                }
                RESIZE_BYTE => Some(ClientEvent::Resize(data.slice(1..))),
                x => {
                    debug!("Ignoring message for unknown channel {x}");
                    None
                }
            };
        }

        // Before v5 there is no close signal, only stdin and resize events
        // come from the client.
        match channel::decode(self.base64, data)? {
            (STDIN_BYTE, payload) => Stdin::data(payload).map(ClientEvent::Stdin),
            (RESIZE_BYTE, payload) => Some(ClientEvent::Resize(payload)),
            (channel, _) => {
                trace!("Ignoring message for channel {channel}");
                None
            }
        }
    }
}

/// What a client sends during a session.
#[derive(Debug, PartialEq)]
pub enum ClientEvent {
    Stdin(Stdin),

    /// A resize event as JSON.
    Resize(Bytes),

    /// The client closed the connection cleanly.
    Closed,
}

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
}

/// The input of a WebSocket client.
pub struct WebSocketInput {
    pub messages: SplitStream<WebSocket>,
    pub framing: Framing,

    /// Touched by every message of the client except for pings and pongs,
    /// which a client like the WebSocket executor of client-go sends as a
    /// heartbeat. CRI-O does not count them either.
    pub activity: Activity,
}

impl ClientInput for WebSocketInput {
    async fn next(&mut self) -> Option<ClientEvent> {
        loop {
            match self.messages.next().await {
                Some(Ok(message)) => {
                    if !matches!(message, Message::Ping(_) | Message::Pong(_)) {
                        self.activity.touch();
                    }
                    if let Some(event) = self.framing.event(message) {
                        return Some(event);
                    }
                }
                _ => return None,
            }
        }
    }
}

/// The input of a SPDY client: the stdin stream, whose end closes stdin, and
/// the resize stream, which carries consecutive JSON objects. Clients open a
/// resize stream only with a terminal, which the server does not know for an
/// attach, so it may also come after the session started.
pub struct SpdyInput {
    stdin: Option<StreamReceiver>,
    resize: Option<StreamReceiver>,
    resize_events: ResizeEvents,

    /// The connection, for the streams which come after the start, see
    /// [`SpdyInput::late_stream`].
    connection: Option<Connection>,

    /// Cancelled once the connection ended.
    done: CancellationToken,
}

/// What a SPDY client sent.
enum SpdyEvent {
    Stdin(Option<Bytes>),
    Resize(Option<Bytes>),
    Stream(Option<Stream>),
    Done,
}

impl SpdyInput {
    async fn recv(receiver: Option<&mut StreamReceiver>) -> Option<Bytes> {
        match receiver {
            Some(receiver) => receiver.recv().await,
            None => future::pending().await,
        }
    }

    async fn accept(connection: Option<&mut Connection>) -> Option<Stream> {
        match connection {
            Some(connection) => connection.accept().await,
            None => future::pending().await,
        }
    }

    /// Take a stream which comes after the start: the resize stream, and
    /// output streams the session does not use, which end right away, like
    /// stderr which clients open for a terminal without asking for one.
    /// Others get reset. Nothing gets awaited, the caller may cancel
    /// [`ClientInput::next`].
    fn late_stream(&mut self, stream: Stream) {
        match stream.headers().get(STREAM_TYPE_HEADER) {
            Some(STREAM_TYPE_RESIZE) if self.resize.is_none() => {
                let (receiver, sender) = stream.split();
                Self::close_later(sender);
                self.resize = Some(receiver);
            }
            Some("stdout" | "stderr") => Self::close_later(stream.split().1),
            _ => debug!("Rejecting a stream opened after the start of the session"),
        }
    }

    /// Close the sending side of a stream without waiting for the queue of
    /// the connection.
    fn close_later(mut sender: StreamSender) {
        tokio::spawn(async move {
            if let Err(e) = sender.close().await {
                debug!("Unable to close the sending side of a stream: {e:#}");
            }
        });
    }
}

/// The events of the resize stream of a SPDY client, which carries JSON
/// objects back to back.
#[derive(Debug, Default)]
pub struct ResizeEvents {
    buf: BytesMut,
    events: VecDeque<Bytes>,
}

impl ResizeEvents {
    /// Split what the client sent into its JSON objects. A broken one drops
    /// what the client sent so far.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        let mut stream = serde_json::Deserializer::from_slice(&self.buf).into_iter::<IgnoredAny>();
        let mut start = 0;
        loop {
            match stream.next() {
                Some(Ok(_)) => {
                    let end = stream.byte_offset();
                    let object = self.buf[start..end].trim_ascii();
                    self.events.push_back(Bytes::copy_from_slice(object));
                    start = end;
                }
                Some(Err(e)) if e.is_eof() => break,
                Some(Err(e)) => {
                    debug!("Dropping invalid resize events: {e}");
                    start = self.buf.len();
                    break;
                }
                None => break,
            }
        }
        let _ = self.buf.split_to(start);
        let whitespace = self
            .buf
            .iter()
            .take_while(|b| b.is_ascii_whitespace())
            .count();
        let _ = self.buf.split_to(whitespace);
        if self.buf.len() > MAX_RESIZE_BUFFER {
            debug!("Dropping incomplete resize event");
            self.buf.clear();
        }
    }

    pub fn pop(&mut self) -> Option<Bytes> {
        self.events.pop_front()
    }
}

impl ClientInput for SpdyInput {
    async fn next(&mut self) -> Option<ClientEvent> {
        loop {
            if let Some(event) = self.resize_events.pop() {
                return Some(ClientEvent::Resize(event));
            }
            let event = select! {
                data = Self::recv(self.stdin.as_mut()) => SpdyEvent::Stdin(data),
                data = Self::recv(self.resize.as_mut()) => SpdyEvent::Resize(data),
                stream = Self::accept(self.connection.as_mut()) => SpdyEvent::Stream(stream),
                () = self.done.cancelled() => SpdyEvent::Done,
            };
            match event {
                SpdyEvent::Stdin(Some(data)) => {
                    if let Some(stdin) = Stdin::data(data) {
                        return Some(ClientEvent::Stdin(stdin));
                    }
                }
                // The client closed stdin, unless the stream got reset.
                SpdyEvent::Stdin(None) => {
                    let reset = self.stdin.take().is_some_and(|s| s.is_reset());
                    if !reset {
                        return Some(ClientEvent::Stdin(Stdin::Close));
                    }
                }
                SpdyEvent::Resize(Some(data)) => self.resize_events.push(&data),
                SpdyEvent::Resize(None) => self.resize = None,
                SpdyEvent::Stream(Some(stream)) => self.late_stream(stream),
                SpdyEvent::Stream(None) => self.connection = None,
                SpdyEvent::Done => return None,
            }
        }
    }
}

/// The receiving side of a stream, whose sending side the server does not
/// need.
async fn receiver(stream: Option<Stream>) -> Option<StreamReceiver> {
    let (receiver, mut sender) = stream?.split();
    if let Err(e) = sender.close().await {
        debug!("Unable to close the sending side of a stream: {e:#}");
    }
    Some(receiver)
}

/// The streams a session expects from a SPDY client. A resize stream is
/// optional, see [`SpdyInput`].
#[derive(Clone, Copy, Debug)]
pub struct ExpectedStreams {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

/// The streams of a SPDY session, which the client opened until the session
/// started.
pub struct SpdyStreams {
    error: Stream,
    stdin: Option<Stream>,
    stdout: Option<Stream>,
    stderr: Option<Stream>,
    resize: Option<Stream>,
}

impl SpdyStreams {
    /// Whether the client may open a stream, by its headers.
    pub fn accept(headers: &Headers, version: Version) -> Result<()> {
        match headers.get(STREAM_TYPE_HEADER) {
            Some("error" | "stdin" | "stdout" | "stderr") => Ok(()),
            Some(STREAM_TYPE_RESIZE) if version.spdy_resize() => Ok(()),
            Some(typ) => bail!("unexpected stream type {:?}", ClientStr(typ)),
            None => bail!("{STREAM_TYPE_HEADER:?} header is required"),
        }
    }

    /// Wait for the streams the session expects, others may come along.
    pub async fn wait(connection: &mut Connection, expected: ExpectedStreams) -> Result<Self> {
        let mut error = None;
        let (mut stdin, mut stdout, mut stderr, mut resize) = (None, None, None, None);
        let timeout = time::sleep(STREAM_CREATION_TIMEOUT);
        tokio::pin!(timeout);
        loop {
            if let Some(error) = error.take_if(|_| {
                (!expected.stdin || stdin.is_some())
                    && (!expected.stdout || stdout.is_some())
                    && (!expected.stderr || stderr.is_some())
            }) {
                return Ok(Self {
                    error,
                    stdin,
                    stdout,
                    stderr,
                    resize,
                });
            }
            let stream = select! {
                stream = connection.accept() => {
                    stream.context("connection ended before the client created all streams")?
                }
                () = &mut timeout => bail!("timed out waiting for client to create streams"),
            };
            let slot = match stream.headers().get(STREAM_TYPE_HEADER) {
                Some("error") => &mut error,
                Some("stdin") => &mut stdin,
                Some("stdout") => &mut stdout,
                Some("stderr") => &mut stderr,
                _ => &mut resize,
            };
            if slot.is_some() {
                // Dropping it resets it.
                debug!("Rejecting a second stream of the same type");
                continue;
            }
            *slot = Some(stream);
        }
    }

    /// The input and the output of the session. The server sends nothing on
    /// the streams of the client. The sending side of stdin stays open, a
    /// client takes its end as the end of stdin, except for v1 like for the
    /// kubelet. The other sending sides get closed.
    pub async fn into_parts(
        self,
        connection: Connection,
        version: Version,
    ) -> (SpdyInput, SpdyOutput) {
        let (stdin, stdin_sender) = match self.stdin.map(Stream::split) {
            Some((receiver, mut sender)) if version == Version::V1 => {
                if let Err(e) = sender.close().await {
                    debug!("Unable to close the sending side of stdin: {e:#}");
                }
                (Some(receiver), None)
            }
            Some((receiver, sender)) => (Some(receiver), Some(sender)),
            None => (None, None),
        };
        let sender = |stream: Option<Stream>| stream.map(|s| s.split().1);
        let output = SpdyOutput {
            version,
            error: self.error.split().1,
            stdin: stdin_sender,
            stdout: sender(self.stdout),
            stderr: sender(self.stderr),
            connection: connection.handle(),
        };
        let input = SpdyInput {
            stdin,
            resize: receiver(self.resize).await,
            resize_events: ResizeEvents::default(),
            done: connection.done(),
            connection: Some(connection),
        };
        (input, output)
    }
}

/// The sending sides of a SPDY session.
pub struct SpdyOutput {
    version: Version,
    error: StreamSender,
    stdin: Option<StreamSender>,
    stdout: Option<StreamSender>,
    stderr: Option<StreamSender>,
    connection: ConnectionHandle,
}

/// The final status of a session.
#[derive(Debug, PartialEq)]
pub enum Status {
    Success,
    ExitCode(i32),

    /// The session itself failed, for example a command which could not be
    /// started.
    Error(String),
}

impl Status {
    /// The status of a command which exited with `code`. A negative one
    /// tells that it could not be waited for, clients only take exit codes
    /// from 1 to 255.
    pub fn exited(code: i32) -> Self {
        match code {
            0 => Self::Success,
            1.. => Self::ExitCode(code),
            _ => Self::Error(format!(
                "unable to get the exit code of the command ({code})"
            )),
        }
    }

    /// The status on the error stream, none for a success before v4.
    fn encode(&self, version: Version) -> Result<Option<Vec<u8>>> {
        if version.json_status() {
            return match self {
                Self::Success => Ok(Some(SUCCESS_STATUS.to_vec())),
                Self::ExitCode(code) => Self::json(&ErrorMessage::new(code)).map(Some),
                Self::Error(message) => {
                    Self::json(&ErrorMessage::internal(message.clone())).map(Some)
                }
            };
        }
        Ok(match self {
            Self::Success => None,
            Self::ExitCode(code) => Some(
                format!("command terminated with non-zero exit code: exit status {code}")
                    .into_bytes(),
            ),
            Self::Error(message) => Some(message.clone().into_bytes()),
        })
    }

    fn json(status: &impl Serialize) -> Result<Vec<u8>> {
        serde_json::to_vec(status).context("serialize status message")
    }
}

#[derive(Debug, Serialize)]
/// Error message type used for exec in case that the command fails.
pub struct ErrorMessage {
    status: &'static str,
    reason: &'static str,
    details: ErrorDetails,
    message: String,
}

impl ErrorMessage {
    pub fn new<T>(exit_code: T) -> Self
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
    pub fn internal(message: String) -> Self {
        Self {
            status: "Failure",
            reason: "InternalError",
            details: ErrorDetails { causes: vec![] },
            message,
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

/// The sending half of a session. A failing send means that the client went
/// away, which ends the session. After a clean close of a WebSocket client,
/// sends fail as well, the output gets dropped then. The session still
/// forwards the stdin which the client sent before its close to the process
/// and reads the output meanwhile, so that the process does not wait for it.
/// SPDY clients do not close cleanly, they close stdin.
pub struct Output {
    transport: Transport,
    failed: bool,

    /// Set once the client closed the connection cleanly.
    peer_closed: Arc<AtomicBool>,
}

enum Transport {
    WebSocket {
        sender: SplitSink<WebSocket, Message>,
        framing: Framing,

        /// Touched by every message with data which got sent, not by pings
        /// and the close.
        activity: Activity,
    },
    Spdy(SpdyOutput),
}

impl Output {
    pub fn websocket(
        sender: SplitSink<WebSocket, Message>,
        framing: Framing,
        activity: Activity,
    ) -> Self {
        Self::new(Transport::WebSocket {
            sender,
            framing,
            activity,
        })
    }

    pub fn spdy(output: SpdyOutput) -> Self {
        Self::new(Transport::Spdy(output))
    }

    fn new(transport: Transport) -> Self {
        Self {
            transport,
            failed: false,
            peer_closed: Arc::default(),
        }
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

    /// Send the final status of the session, a client which does not take it
    /// in time does not get it.
    pub async fn status(&mut self, status: &Status) -> Result<()> {
        time::timeout(SHUTDOWN_TIMEOUT, self.send_status(status))
            .await
            .unwrap_or_else(|_| Err(anyhow!("client did not take the status in time")))
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
                bail!("SPDY connection closed")
            }
            Transport::Spdy(_) => Ok(()),
        }
    }

    /// End the session: a WebSocket gets closed, the SPDY streams end and the
    /// writer flushes what is queued, shuts the connection down and ends it.
    /// A client which does not read gets the connection ended after
    /// [`SHUTDOWN_TIMEOUT`]. Whether the close went out, which the client of
    /// a WebSocket answers.
    pub async fn close(&mut self) -> bool {
        let close = async {
            match &mut self.transport {
                Transport::WebSocket { .. } => {
                    if let Err(e) = self.send(web::close_message("done")).await {
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
        let closed = time::timeout(SHUTDOWN_TIMEOUT, close)
            .await
            .unwrap_or_else(|_| {
                debug!("Session did not flush in time");
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

    fn sent(&mut self, result: Result<()>) -> Result<()> {
        match result {
            Ok(()) => Ok(()),
            Err(e) if self.peer_closed.load(Ordering::Acquire) => {
                debug!("Dropping the output of the session after the close: {e:#}");
                self.failed = true;
                Ok(())
            }
            Err(e) => Err(e).context("send to client"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spdy::testing::{Client, Frame};
    use tokio::io::duplex;

    #[test]
    fn websocket_protocols_get_negotiated() {
        let negotiate = |requested: &[&str]| Framing::negotiate(requested.iter().copied());
        let v1 = Framing {
            version: Version::V1,
            base64: false,
        };
        assert_eq!(negotiate(&[]), Some((v1, None)));
        assert_eq!(
            negotiate(&["v5.channel.k8s.io", "v4.channel.k8s.io"]),
            Some((Framing::V5, Some("v5.channel.k8s.io")))
        );
        // The order of the client decides.
        assert_eq!(
            negotiate(&["unknown", "v4.base64.channel.k8s.io", "v5.channel.k8s.io"]),
            Some((
                Framing {
                    version: Version::V4,
                    base64: true
                },
                Some("v4.base64.channel.k8s.io")
            ))
        );
        assert_eq!(
            negotiate(&["base64.channel.k8s.io"]),
            Some((
                Framing {
                    version: Version::V1,
                    base64: true
                },
                Some("base64.channel.k8s.io")
            ))
        );
        assert_eq!(negotiate(&["v3.channel.k8s.io"]), None);
    }

    #[test]
    fn spdy_versions() {
        assert_eq!(spdy_version("v4.channel.k8s.io"), Some(Version::V4));
        assert_eq!(spdy_version("channel.k8s.io"), Some(Version::V1));
        assert_eq!(spdy_version("v5.channel.k8s.io"), None);
        assert!(Version::V3.spdy_resize());
        assert!(!Version::V2.spdy_resize());
    }

    #[test]
    fn v5_events() {
        let event =
            |data: &'static [u8]| Framing::V5.event(Message::Binary(Bytes::from_static(data)));
        assert_eq!(
            event(&[STDIN_BYTE, b'l', b's']),
            Some(ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"ls"))))
        );
        // Empty stdin carries nothing.
        assert_eq!(event(&[STDIN_BYTE]), None);
        assert_eq!(
            event(&[CLOSE_BYTE, STDIN_BYTE]),
            Some(ClientEvent::Stdin(Stdin::Close))
        );
        // A close needs the stdin channel, others cannot be closed.
        assert_eq!(event(&[CLOSE_BYTE]), None);
        assert_eq!(event(&[CLOSE_BYTE, RESIZE_BYTE]), None);
        assert_eq!(event(&[CLOSE_BYTE, STDIN_BYTE, STDIN_BYTE]), None);
        assert_eq!(
            event(&[RESIZE_BYTE, b'{', b'}']),
            Some(ClientEvent::Resize(Bytes::from_static(b"{}")))
        );
        // Unknown and empty messages are ignored.
        assert_eq!(event(&[42, b'x']), None);
        assert_eq!(event(&[]), None);
        assert_eq!(Framing::V5.event(Message::Text("0a".into())), None);
        assert_eq!(
            Framing::V5.event(Message::Close(None)),
            Some(ClientEvent::Closed)
        );
    }

    #[test]
    fn older_events_have_no_close_signal() {
        let binary = Framing {
            version: Version::V4,
            base64: false,
        };
        let event = |data: &'static [u8]| binary.event(Message::Binary(Bytes::from_static(data)));
        assert_eq!(
            event(b"\x00ls"),
            Some(ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"ls"))))
        );
        assert_eq!(
            event(b"\x04{}"),
            Some(ClientEvent::Resize(Bytes::from_static(b"{}")))
        );
        // The close signal of v5, the output channels and empty stdin are
        // ignored.
        assert_eq!(event(&[CLOSE_BYTE, STDIN_BYTE]), None);
        assert_eq!(event(b"\x01out"), None);
        assert_eq!(event(&[STDIN_BYTE]), None);

        let base64 = Framing {
            version: Version::V1,
            base64: true,
        };
        assert_eq!(
            base64.event(Message::Text("0bHM=".into())),
            Some(ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"ls"))))
        );
        assert_eq!(
            base64.event(Message::Binary(Bytes::from_static(b"4e30="))),
            Some(ClientEvent::Resize(Bytes::from_static(b"{}")))
        );
        assert_eq!(base64.event(Message::Text("0!".into())), None);
        assert_eq!(base64.event(Message::Text("0".into())), None);
    }

    #[test]
    fn statuses_depend_on_the_version() -> Result<()> {
        assert_eq!(
            Status::Success.encode(Version::V4)?.as_deref(),
            Some(SUCCESS_STATUS)
        );
        assert_eq!(Status::Success.encode(Version::V3)?, None);
        assert_eq!(
            Status::ExitCode(137).encode(Version::V5)?,
            Some(serde_json::to_vec(&ErrorMessage::new(137))?)
        );
        assert_eq!(
            Status::ExitCode(137).encode(Version::V2)?.as_deref(),
            Some(&b"command terminated with non-zero exit code: exit status 137"[..])
        );
        assert_eq!(
            Status::Error("failed".into())
                .encode(Version::V1)?
                .as_deref(),
            Some(&b"failed"[..])
        );
        Ok(())
    }

    #[test]
    fn exit_statuses() -> Result<()> {
        assert_eq!(
            Status::exited(0).encode(Version::V5)?.as_deref(),
            Some(SUCCESS_STATUS)
        );
        assert_eq!(
            Status::exited(137).encode(Version::V5)?,
            Some(serde_json::to_vec(&ErrorMessage::new(137))?)
        );
        // Clients cannot parse a negative exit code, it is an error instead.
        let internal = serde_json::to_vec(&ErrorMessage::internal(
            "unable to get the exit code of the command (-3)".into(),
        ))?;
        assert_eq!(Status::exited(-3).encode(Version::V4)?, Some(internal));
        assert_eq!(
            Status::exited(-1).encode(Version::V2)?.as_deref(),
            Some(&b"unable to get the exit code of the command (-1)"[..])
        );
        Ok(())
    }

    #[test]
    fn success_status() {
        assert_eq!(SUCCESS_STATUS, b"{\"metadata\":{},\"status\":\"Success\"}");
        assert_eq!(
            Framing::V5.message(STREAM_ERR_BYTE, SUCCESS_STATUS),
            Message::Binary(Bytes::from_static(
                b"\x03{\"metadata\":{},\"status\":\"Success\"}"
            ))
        );
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
    fn resize_streams_get_split_into_events() {
        let mut input = ResizeEvents::default();
        // Go encodes one object per line, which may arrive in parts.
        input.push(b"{\"Width\":80,\"Height\":24}\n{\"Width\":");
        input.push(b"100,\"Height\":40}\n");
        assert_eq!(
            input.events,
            [
                Bytes::from_static(b"{\"Width\":80,\"Height\":24}"),
                Bytes::from_static(b"{\"Width\":100,\"Height\":40}"),
            ]
        );
        assert!(input.buf.is_empty());

        // Garbage gets dropped, later events still work.
        input.events.clear();
        input.push(b"}{\"Width\":1,\"Height\":2}");
        input.push(b"{\"Width\":1,\"Height\":2}");
        assert_eq!(
            input.events,
            [Bytes::from_static(b"{\"Width\":1,\"Height\":2}")]
        );

        // An object which never ends is bounded.
        input.push(&vec![b'['; MAX_RESIZE_BUFFER + 1]);
        assert!(input.buf.is_empty());
    }

    #[tokio::test]
    async fn late_resize_streams_survive_a_cancelled_next() -> Result<()> {
        let (client, server) = duplex(1024);
        let mut connection =
            Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
        let mut client = Client::new(client)?;
        client.open(1, [("streamType", "error")].into()).await?;
        client.open(3, [("streamType", "stdout")].into()).await?;
        let streams = SpdyStreams::wait(
            &mut connection,
            ExpectedStreams {
                stdin: false,
                stdout: true,
                stderr: false,
            },
        )
        .await?;
        let (mut input, output) = streams.into_parts(connection, Version::V4).await;
        let mut output = Output::spdy(output);

        // The client does not read, the data queue of the connection fills.
        let data = Bytes::from(vec![0; 32 * 1024]);
        let fill = async { while output.data(STDOUT_BYTE, &data).await.is_ok() {} };
        assert!(
            time::timeout(Duration::from_millis(500), fill)
                .await
                .is_err()
        );

        // The resize stream comes meanwhile, the session cancels next() like
        // when its stdin channel gets a slot first.
        client.open(5, [("streamType", "resize")].into()).await?;
        assert!(
            time::timeout(Duration::from_millis(500), input.next())
                .await
                .is_err()
        );

        // The stream still works once the client reads again.
        client
            .send(Frame::Data {
                stream_id: 5,
                fin: false,
                data: Bytes::from_static(b"{\"Width\":1,\"Height\":2}\n"),
            })
            .await?;
        // The resets the client got, until it got nothing for a second.
        let resets = tokio::spawn(async move {
            let mut resets = vec![];
            while let Ok(Ok(Some(frame))) =
                time::timeout(Duration::from_secs(1), client.frame()).await
            {
                if let Frame::RstStream { stream_id, .. } = frame {
                    resets.push(stream_id);
                }
            }
            resets
        });
        assert_eq!(
            time::timeout(Duration::from_secs(5), input.next()).await?,
            Some(ClientEvent::Resize(Bytes::from_static(
                b"{\"Width\":1,\"Height\":2}"
            )))
        );
        let resets = time::timeout(Duration::from_secs(10), resets).await??;
        assert!(!resets.contains(&5), "{resets:?}");
        drop((input, output));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn closing_a_session_of_a_client_which_does_not_read_ends() -> Result<()> {
        let (client, server) = duplex(1024);
        let mut connection =
            Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
        let mut client = Client::new(client)?;
        client.open(1, [("streamType", "error")].into()).await?;
        client.open(3, [("streamType", "stdout")].into()).await?;
        let streams = SpdyStreams::wait(
            &mut connection,
            ExpectedStreams {
                stdin: false,
                stdout: true,
                stderr: false,
            },
        )
        .await?;
        let (_input, output) = streams.into_parts(connection, Version::V4).await;
        let mut output = Output::spdy(output);

        // The client does not read, the output backs up.
        let data = Bytes::from(vec![0; 32 * 1024]);
        let backed_up = async { while output.data(STDOUT_BYTE, &data).await.is_ok() {} };
        assert!(
            time::timeout(Duration::from_secs(1), backed_up)
                .await
                .is_err()
        );

        // The end of the session does not wait for the client forever.
        time::timeout(3 * SHUTDOWN_TIMEOUT, async {
            assert!(output.status(&Status::Success).await.is_err());
            assert!(!output.close().await);
        })
        .await?;
        drop(client);
        Ok(())
    }
}
