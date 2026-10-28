//! Exec and attach over SPDY: the protocol versions, the streams of a
//! session and the input of the client.

use super::{ClientEvent, ClientInput, MAX_RESIZE_BUFFER, STREAM_TYPE_RESIZE, Stdin, Version};
use crate::streaming_server::{
    spdy::{Connection, ConnectionHandle, Headers, Stream, StreamReceiver, StreamSender},
    stream_protocol::{ClientStr, STREAM_CREATION_TIMEOUT, STREAM_TYPE_HEADER},
};
use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use serde::de::IgnoredAny;
use std::{
    future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{select, time};
use tokio_util::sync::CancellationToken;
use tracing::debug;

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

/// The input of a SPDY client: the stdin stream, whose end closes stdin, and
/// the resize stream, which carries consecutive JSON objects. Clients open a
/// resize stream only with a terminal, which the server does not know for an
/// attach, so it may also come after the session started.
pub struct SpdyInput {
    stdin: Option<StreamReceiver>,

    /// Whether the client closed stdin cleanly. The end of the connection is
    /// a clean close then, which still forwards the stdin the client sent
    /// before.
    stdin_ended: bool,

    /// Set once the client closed its stdin, so that the output, which fails
    /// once the connection ended, does not end the session before the stdin
    /// got forwarded, see [`Output`](super::Output).
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
                tokio::spawn(close_sending_side(sender, "a stream"));
                self.resize = Some(receiver);
            }
            Some("stdout" | "stderr") => {
                tokio::spawn(close_sending_side(stream.split().1, "a stream"));
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
                data = Self::recv(self.resize.as_mut()) => SpdyEvent::Resize(data),
                data = Self::recv(self.stdin.as_mut()) => SpdyEvent::Stdin(data),
                stream = Self::accept(self.connection.as_mut()) => SpdyEvent::Stream(stream),
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
                        self.stdin_ended = true;
                        self.peer_closed.store(true, Ordering::Release);
                        return Some(ClientEvent::Stdin(Stdin::Close));
                    }
                }
                SpdyEvent::Resize(Some(data)) => self.resize_events.push(&data),
                SpdyEvent::Resize(None) => self.resize = None,
                SpdyEvent::Stream(Some(stream)) => self.late_stream(stream),
                SpdyEvent::Stream(None) => self.connection = None,
                SpdyEvent::Done => return self.stdin_ended.then_some(ClientEvent::Closed),
            }
        }
    }
}

/// The receiving side of a stream, whose sending side the server does not
/// need.
async fn receiver(stream: Option<Stream>) -> Option<StreamReceiver> {
    let (receiver, sender) = stream?.split();
    close_sending_side(sender, "a stream").await;
    Some(receiver)
}

/// Close the sending side of a stream, `what` names it in the log. A late
/// stream gets it in a task of its own, without waiting for the queue of the
/// connection.
async fn close_sending_side(mut sender: StreamSender, what: &str) {
    if let Err(e) = sender.close().await {
        debug!("Unable to close the sending side of {what}: {e:#}");
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
        match headers.get(STREAM_TYPE_HEADER) {
            Some("error" | "stdin" | "stdout" | "stderr") => Ok(()),
            Some(STREAM_TYPE_RESIZE) if version.spdy_resize() => Ok(()),
            Some(typ) => bail!("unexpected stream type {:?}", ClientStr(typ)),
            None => bail!("{STREAM_TYPE_HEADER:?} header is required"),
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
            Some((receiver, sender)) if version == Version::V1 => {
                close_sending_side(sender, "stdin").await;
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
            stdin_ended: false,
            peer_closed,
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
    /// Shared with the input, see [`Output::spdy`](super::Output::spdy).
    pub(super) peer_closed: Arc<AtomicBool>,
    pub(super) version: Version,
    pub(super) error: StreamSender,
    pub(super) stdin: Option<StreamSender>,
    pub(super) stdout: Option<StreamSender>,
    pub(super) stderr: Option<StreamSender>,
    pub(super) connection: ConnectionHandle,
}
