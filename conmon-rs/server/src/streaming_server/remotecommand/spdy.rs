//! Exec and attach over SPDY: the protocol versions, the streams of a
//! session and the input of the client.

use super::{ClientEvent, ClientInput, Stdin, Version};
use crate::streaming_server::{
    spdy::{Connection, ConnectionHandle, Headers, Stream, StreamReceiver, StreamSender},
    stream_protocol::{ClientStr, STREAM_CREATION_TIMEOUT, STREAM_TYPE_HEADER},
};
use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use serde::de::IgnoredAny;
use std::{
    future::{self, Future},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{select, time};
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// The most resize events a SPDY client may send without completing one.
pub(super) const MAX_RESIZE_BUFFER: usize = 4 * 1024;

pub(super) const STREAM_TYPE_RESIZE: &str = "resize";

/// The SPDY protocols, the kubelet and client-go prefer the first one.
pub const SPDY_PROTOCOLS: [(&str, Version); 4] = [
    ("v4.channel.k8s.io", Version::V4),
    ("v3.channel.k8s.io", Version::V3),
    ("v2.channel.k8s.io", Version::V2),
    ("channel.k8s.io", Version::V1),
];

/// The input of a SPDY client: the stdin stream, whose end closes stdin, and
/// the resize stream, which carries consecutive JSON objects. Clients open a
/// resize stream only with a terminal, which the server does not know for an
/// attach, so it may also come after the session started. Both get read
/// together: once the stdin which the process does not read fills
/// `PENDING_STDIN_MAX_BYTES` of the session, the resizes wait as well.
pub struct SpdyInput {
    stdin: Option<StreamReceiver>,

    /// Set once the client closed its stdin cleanly, so that the output,
    /// which fails once the connection ended, does not end the session
    /// before the stdin got forwarded, see [`Output`](super::Output). The
    /// end of the connection is a clean close then, which still forwards the
    /// stdin the client sent before.
    peer_closed: Arc<AtomicBool>,
    resize: Option<StreamReceiver>,
    resize_events: ResizeEvents,

    /// The connection, for the streams which come after the start, see
    /// [`SpdyInput::late_stream`].
    connection: Option<Connection>,

    /// Cancelled once the connection ended.
    pub(super) done: CancellationToken,
}

/// What a SPDY client sent.
enum SpdyEvent {
    Stdin(Option<Bytes>),
    Resize(Option<Bytes>),
    Stream(Option<Stream>),
    Done,
}

/// The output of `f` for the value, pending without one, like for a stream
/// which ended.
async fn or_pending<T, F>(value: Option<T>, f: impl FnOnce(T) -> F) -> F::Output
where
    F: Future,
{
    match value {
        Some(value) => f(value).await,
        None => future::pending().await,
    }
}

impl SpdyInput {
    /// Take a stream which comes after the start: the resize stream, and
    /// output streams the session does not use, which end right away, like
    /// stderr which clients open for a terminal without asking for one.
    /// Others get reset. Nothing gets awaited, the caller may cancel
    /// [`ClientInput::next`].
    fn late_stream(&mut self, stream: Stream) {
        match StreamType::parse(stream.headers()) {
            Ok(StreamType::Resize) if self.resize.is_none() => {
                let (receiver, sender) = stream.split();
                tokio::spawn(close_sending_side(sender));
                self.resize = Some(receiver);
            }
            Ok(StreamType::Stdout | StreamType::Stderr) => {
                tokio::spawn(close_sending_side(stream.split().1));
            }
            _ => debug!("Rejecting a stream opened after the start of the session"),
        }
    }
}

/// The events of the resize stream of a SPDY client, which carries JSON
/// objects back to back. Only the latest one matters, the earlier sizes are
/// gone already, so a client sending many at once does not queue them.
#[derive(Debug, Default)]
pub struct ResizeEvents {
    pub(super) buf: BytesMut,
    latest: Option<Bytes>,
}

impl ResizeEvents {
    /// Split what the client sent into its JSON objects and keep the last
    /// one. A broken one drops what the client sent so far.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        let mut stream = serde_json::Deserializer::from_slice(&self.buf).into_iter::<IgnoredAny>();
        let mut start = 0;
        let mut latest = None;
        loop {
            match stream.next() {
                Some(Ok(_)) => {
                    let end = stream.byte_offset();
                    latest = Some(start..end);
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
        if let Some(range) = latest {
            self.latest = Some(Bytes::copy_from_slice(self.buf[range].trim_ascii()));
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
        self.latest.take()
    }
}

impl ClientInput for SpdyInput {
    async fn next(&mut self) -> Option<ClientEvent> {
        loop {
            if let Some(event) = self.resize_events.pop() {
                return Some(ClientEvent::Resize(event));
            }
            // The connection is done once its reader ended, which happens
            // before the data queued on its streams got taken. What the
            // client sent before closing still goes first. The rare resizes
            // go before stdin, which a busy client keeps ready.
            let event = select! {
                biased;
                data = or_pending(self.resize.as_mut(), StreamReceiver::recv) => SpdyEvent::Resize(data),
                data = or_pending(self.stdin.as_mut(), StreamReceiver::recv) => SpdyEvent::Stdin(data),
                stream = or_pending(self.connection.as_mut(), Connection::accept) => SpdyEvent::Stream(stream),
                () = self.done.cancelled() => SpdyEvent::Done,
            };
            match event {
                SpdyEvent::Stdin(Some(data)) => {
                    if let Some(stdin) = Stdin::data(data) {
                        return Some(ClientEvent::Stdin(stdin));
                    }
                }
                // The client closed stdin, unless the stream got reset, also
                // if the connection ended right after.
                SpdyEvent::Stdin(None) => {
                    let reset = self
                        .stdin
                        .take()
                        .is_some_and(|s| s.is_reset() && !s.is_closed_by_client());
                    if !reset {
                        self.peer_closed.store(true, Ordering::Release);
                        return Some(ClientEvent::Stdin(Stdin::Close));
                    }
                }
                SpdyEvent::Resize(Some(data)) => self.resize_events.push(&data),
                SpdyEvent::Resize(None) => self.resize = None,
                SpdyEvent::Stream(Some(stream)) => self.late_stream(stream),
                SpdyEvent::Stream(None) => self.connection = None,
                // The session sets it as well, but only once the input ended
                // with the close, so it tells whether stdin got closed.
                SpdyEvent::Done => {
                    return self
                        .peer_closed
                        .load(Ordering::Acquire)
                        .then_some(ClientEvent::Closed);
                }
            }
        }
    }
}

/// The receiving side of a stream, whose sending side the server does not
/// need.
async fn receiver(stream: Option<Stream>) -> Option<StreamReceiver> {
    let (receiver, sender) = stream?.split();
    close_sending_side(sender).await;
    Some(receiver)
}

/// Close the sending side of a stream. A late stream gets it in a task of
/// its own, without waiting for the queue of the connection.
async fn close_sending_side(mut sender: StreamSender) {
    if let Err(e) = sender.close().await {
        debug!("Unable to close the sending side of a stream: {e:#}");
    }
}

/// The type of a stream of a session, by its header.
#[derive(Clone, Copy, Debug, PartialEq)]
enum StreamType {
    Error,
    Stdin,
    Stdout,
    Stderr,
    Resize,
}

impl StreamType {
    fn parse(headers: &Headers) -> Result<Self> {
        match headers.get(STREAM_TYPE_HEADER) {
            Some("error") => Ok(Self::Error),
            Some("stdin") => Ok(Self::Stdin),
            Some("stdout") => Ok(Self::Stdout),
            Some("stderr") => Ok(Self::Stderr),
            Some(STREAM_TYPE_RESIZE) => Ok(Self::Resize),
            Some(typ) => bail!("unexpected stream type {:?}", ClientStr(typ)),
            None => bail!("{STREAM_TYPE_HEADER:?} header is required"),
        }
    }
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
        match StreamType::parse(headers)? {
            StreamType::Resize if !version.spdy_resize() => {
                bail!("unexpected stream type {STREAM_TYPE_RESIZE:?}")
            }
            _ => Ok(()),
        }
    }

    /// Wait for the streams the session expects, others may come along.
    /// Nothing reads the streams meanwhile: a client which sends more stdin
    /// than a stream queues before it opened the others holds up the
    /// connection until this times out. client-go opens all streams first.
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
            // Only accepted streams come, see [`Self::accept`]. Another one
            // gets reset like a second stream of a type.
            let slot = match StreamType::parse(stream.headers()) {
                Ok(StreamType::Error) => &mut error,
                Ok(StreamType::Stdin) => &mut stdin,
                Ok(StreamType::Stdout) => &mut stdout,
                Ok(StreamType::Stderr) => &mut stderr,
                Ok(StreamType::Resize) => &mut resize,
                Err(_) => continue,
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
            Some((receiver, sender)) if version == Version::V1 => {
                close_sending_side(sender).await;
                (Some(receiver), None)
            }
            Some((receiver, sender)) => (Some(receiver), Some(sender)),
            None => (None, None),
        };
        let sender = |stream: Option<Stream>| stream.map(|s| s.split().1);
        let peer_closed = Arc::<AtomicBool>::default();
        let output = SpdyOutput {
            peer_closed: peer_closed.clone(),
            version,
            error: self.error.split().1,
            stdin: stdin_sender,
            stdout: sender(self.stdout),
            stderr: sender(self.stderr),
            connection: connection.handle(),
        };
        let input = SpdyInput {
            peer_closed,
            stdin,
            resize: receiver(self.resize).await,
            resize_events: ResizeEvents::default(),
            done: connection.handle().done(),
            connection: Some(connection),
        };
        (input, output)
    }
}

/// The sending sides of a SPDY session.
pub struct SpdyOutput {
    /// Shared with the input, see [`Output::spdy`](super::Output::spdy).
    pub(super) peer_closed: Arc<AtomicBool>,
    pub(super) version: Version,
    pub(super) error: StreamSender,
    pub(super) stdin: Option<StreamSender>,
    pub(super) stdout: Option<StreamSender>,
    pub(super) stderr: Option<StreamSender>,
    pub(super) connection: ConnectionHandle,
}
