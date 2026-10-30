//! The HTTP side of the streaming server on top of hyper: short responses
//! and the upgrade of a request to a WebSocket. The WebSocket handshake
//! checks the request like RFC 6455 says, a request which does not pass gets
//! a plain text response which tells what is missing.

use bytes::Bytes;
use futures::{
    sink::{Sink, SinkExt},
    stream::{SplitSink, SplitStream, Stream, StreamExt},
    task::AtomicWaker,
};
use hyper::{
    Method, Request, StatusCode,
    body::{Body as HttpBody, Frame, SizeHint},
    header::{self, HeaderMap, HeaderName, HeaderValue},
    upgrade::{OnUpgrade, Upgraded},
};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, ready},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    task::JoinHandle,
    time::{self, Duration},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        error::CapacityError,
        handshake::derive_accept_key,
        protocol::{Role, WebSocketConfig},
    },
};
use tracing::debug;

pub use tokio_tungstenite::tungstenite::{
    Error, Message,
    protocol::{CloseFrame, frame::coding::CloseCode},
};

/// A WebSocket of a client, after the upgrade of its request.
pub type WebSocket = WebSocketStream<ClientIo<TokioIo<Upgraded>>>;

/// How long a client gets to take the end of a WebSocket, the close of the
/// server and what is queued before it.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The most a client may still send after the close for a message which was
/// too large, see [`drain`]. More than the rest of a message of the size
/// Kubernetes takes.
const DRAIN_LIMIT: usize = 64 * 1024 * 1024;

/// The buffer a WebSocket reads into, which it allocates up front. Clients
/// mostly send small stdin and resize events, larger messages grow it while
/// they get read. Tungstenite allocates 128 KiB otherwise, for every session.
const READ_BUFFER_SIZE: usize = 16 * 1024;

/// The close of the server once it is done, or after the client sent a
/// message larger than the server takes, see [`is_too_large`], which tells
/// the client why.
pub fn close(too_large: bool, reason: &str) -> Message {
    let (code, reason) = if too_large {
        (CloseCode::Size, "message too large")
    } else {
        (CloseCode::Normal, reason)
    };
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

/// Send the close of the server without a reason, see [`close`], the sink
/// once it went out.
pub async fn send_close<S>(mut sink: S, too_large: bool) -> Option<S>
where
    S: Sink<Message> + Unpin,
{
    sink.send(close(too_large, ""))
        .await
        .is_ok()
        .then_some(sink)
}

/// Whether a read failed for a message larger than the server takes. The
/// WebSocket cannot be read anymore then, the rest of the message did not
/// get read, but the server can still close it, see [`close`], and has to
/// [`drain`] it then.
pub fn is_too_large(e: &Error) -> bool {
    matches!(e, Error::Capacity(CapacityError::MessageTooLong { .. }))
}

/// End a WebSocket whose client sent a message larger than the server
/// takes, once the close went out. The rest of the message cannot be read
/// as messages. The server stops writing and drops what the client still
/// sends until it ends the connection, for up to [`CLOSE_TIMEOUT`] and
/// [`DRAIN_LIMIT`], on top of what got dropped while the close went out, see
/// [`Discard`]. A socket closed with input it did not read resets the
/// connection, which can drop the close and the status before it.
pub async fn drain<T>(
    sink: SplitSink<WebSocketStream<ClientIo<T>>, Message>,
    messages: SplitStream<WebSocketStream<ClientIo<T>>>,
) where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let Ok(mut socket) = messages.reunite(sink) else {
        return;
    };
    let io = socket.get_mut();
    io.drain();
    let drained = async {
        if let Err(e) = io.shutdown().await {
            debug!("Unable to shut down the WebSocket: {e}");
        }
        // Dropped input never reaches the buffer, a read ends with the
        // connection or the limit.
        if let Err(e) = io.read(&mut [0; 1]).await {
            debug!("Stopped draining the WebSocket: {e}");
        }
    };
    if time::timeout(CLOSE_TIMEOUT, drained).await.is_err() {
        debug!("Client did not end the WebSocket in time");
    }
}

/// The connection of a WebSocket client, whose input can get dropped
/// instead of read, see [`Discard`].
pub struct ClientIo<T> {
    io: T,
    discard: Arc<DiscardState>,

    /// How much input may still get dropped.
    left: usize,

    /// Whether the client ended its side of the connection.
    ended: bool,
}

#[derive(Debug, Default)]
struct DiscardState {
    /// Whether the input gets dropped.
    on: AtomicBool,

    /// The task whose write waits for the client, which has to drop its
    /// input from now on.
    writer: AtomicWaker,
}

/// Drops the input of a [`ClientIo`], also after its WebSocket got split. A
/// client which sent a message larger than the server takes may still be
/// busy writing the rest of it and not read meanwhile, which holds up the
/// status and the close for it, unless its input gets read. The WebSocket
/// cannot read it anymore, it fails on the message again. Writes drop what
/// the client sent before they write, up to [`DRAIN_LIMIT`], and the input
/// of the client wakes a write which waits for it. Reads see nothing but the
/// end of the connection.
#[derive(Clone, Debug, Default)]
pub struct Discard(Arc<DiscardState>);

impl Discard {
    pub fn discard(&self) {
        self.0.on.store(true, Ordering::Release);
        // A write which waits for a client busy with the rest of its message
        // would never get woken otherwise.
        self.0.writer.wake();
    }
}

impl<T> ClientIo<T> {
    /// How many reads of dropped input one poll does at most, so that a
    /// client which keeps sending does not hold the thread.
    const DISCARD_READS: usize = 16;

    pub fn new(io: T) -> Self {
        Self {
            io,
            discard: Arc::default(),
            left: DRAIN_LIMIT,
            ended: false,
        }
    }

    /// Drops the input of the connection.
    pub fn discarder(&self) -> Discard {
        Discard(self.discard.clone())
    }

    fn discarding(&self) -> bool {
        self.discard.on.load(Ordering::Acquire)
    }

    /// Drop the input from now on, with a budget of its own for the end of
    /// the WebSocket, see [`drain`].
    fn drain(&mut self) {
        self.discard.on.store(true, Ordering::Release);
        self.left = DRAIN_LIMIT;
    }
}

impl<T: AsyncRead + Unpin> ClientIo<T> {
    /// Read and drop the input until the connection ended, which it is
    /// ready for.
    fn poll_discard(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.ended {
            return Poll::Ready(Ok(()));
        }
        let mut scratch = [0; 8192];
        for _ in 0..Self::DISCARD_READS {
            if self.left == 0 {
                return Poll::Ready(Err(io::Error::other(
                    "client sent too much after the close",
                )));
            }
            let mut dropped = ReadBuf::new(&mut scratch);
            ready!(Pin::new(&mut self.io).poll_read(cx, &mut dropped))?;
            match dropped.filled().len() {
                0 => {
                    self.ended = true;
                    return Poll::Ready(Ok(()));
                }
                read => self.left = self.left.saturating_sub(read),
            }
        }
        // More input may wait, which is readable already and wakes nobody.
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    /// Run a write, after dropping the input if it gets dropped. The read
    /// leaves the waker of the write with the connection, so that input
    /// wakes a write which waits for the client, also one which started
    /// before the input got dropped.
    fn poll_writing<R>(
        &mut self,
        cx: &mut Context<'_>,
        write: impl FnOnce(Pin<&mut T>, &mut Context<'_>) -> Poll<R>,
    ) -> Poll<R> {
        let discarding = self.discarding();
        if discarding {
            // Its failures end the dropping, the write still goes on.
            let _ = self.poll_discard(cx);
        }
        let poll = write(Pin::new(&mut self.io), cx);
        if poll.is_pending() && !discarding {
            self.discard.writer.register(cx.waker());
            if self.discarding() {
                cx.waker().wake_by_ref();
            }
        }
        poll
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ClientIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.discarding() {
            return this.poll_discard(cx);
        }
        Pin::new(&mut this.io).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for ClientIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut()
            .poll_writing(cx, |io, cx| io.poll_write(cx, buf))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut()
            .poll_writing(cx, |io, cx| io.poll_write_vectored(cx, bufs))
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_writing(cx, AsyncWrite::poll_flush)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_writing(cx, AsyncWrite::poll_shutdown)
    }
}

/// Wait for the task which writes to a WebSocket and closes it, abort it
/// after [`CLOSE_TIMEOUT`]. Dropping its handle would leave it running, a
/// writer to a client which does not read would hold the connection. The
/// output of the writer, `None` if it did not end in time.
pub async fn join_or_abort<T>(mut writer: JoinHandle<T>) -> Option<T> {
    if let Ok(output) = time::timeout(CLOSE_TIMEOUT, &mut writer).await {
        output.ok()
    } else {
        writer.abort();
        None
    }
}

/// Read the client after the close of the server, until it answered the
/// close, for up to [`CLOSE_TIMEOUT`]. What it sends meanwhile gets dropped.
/// A socket closed with input it did not read resets the connection, which
/// can drop what the client did not read yet, like the close and the output
/// before it.
pub async fn await_close_answer<S, E>(messages: &mut S)
where
    S: Stream<Item = Result<Message, E>> + Unpin,
{
    let answer = async {
        while let Some(Ok(message)) = messages.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    };
    if time::timeout(CLOSE_TIMEOUT, answer).await.is_err() {
        debug!("Client did not answer the close in time");
    }
}

/// End a WebSocket: wait for the writer, which sends the close of the
/// server, see [`join_or_abort`], then for the answer of the client, see
/// [`await_close_answer`]. The writer returns its sink once its close went
/// out, a client gets no time to answer a close it did not get.
pub async fn finish<S, E, T>(writer: JoinHandle<Option<T>>, messages: &mut S)
where
    S: Stream<Item = Result<Message, E>> + Unpin,
{
    if let Some(Some(_)) = join_or_abort(writer).await {
        await_close_answer(messages).await;
    }
}

/// A response of the streaming server.
pub type Response = hyper::Response<Body>;

/// The body of a response. The server sends short texts only, at once.
#[derive(Debug, Default)]
pub struct Body(Option<Bytes>);

impl From<Bytes> for Body {
    fn from(data: Bytes) -> Self {
        Self((!data.is_empty()).then_some(data))
    }
}

impl HttpBody for Body {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(self.get_mut().0.take().map(|data| Ok(Frame::data(data))))
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.0.as_ref().map_or(0, |data| data.len() as u64))
    }
}

/// A response with the status only.
pub fn status(status: StatusCode) -> Response {
    let mut response = Response::default();
    *response.status_mut() = status;
    response
}

/// A response with a plain text, which tells the client what went wrong.
pub fn text(status: StatusCode, text: impl Into<String>) -> Response {
    let mut response = Response::new(Bytes::from(text.into()).into());
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// A request which asks for a WebSocket, checked like RFC 6455 says.
pub struct WebSocketUpgrade {
    key: HeaderValue,
    on_upgrade: OnUpgrade,
    protocol: Option<&'static str>,
    config: WebSocketConfig,
}

impl WebSocketUpgrade {
    /// Check the request, the rejection is the response to send otherwise.
    /// The server speaks HTTP/1 only, where a WebSocket starts with a GET.
    pub fn from_request<B>(request: &mut Request<B>) -> Result<Self, Box<Response>> {
        let reject = |status, message: &'static str| Err(Box::new(text(status, message)));
        if request.method() != Method::GET {
            return reject(
                StatusCode::METHOD_NOT_ALLOWED,
                "Request method must be `GET`",
            );
        }
        let headers = request.headers();
        if !header_has_token(headers, header::CONNECTION, "upgrade") {
            return reject(
                StatusCode::BAD_REQUEST,
                "Connection header did not include 'upgrade'",
            );
        }
        if !header_has_token(headers, header::UPGRADE, "websocket") {
            return reject(
                StatusCode::BAD_REQUEST,
                "`Upgrade` header did not include 'websocket'",
            );
        }
        let Some(key) = headers.get(header::SEC_WEBSOCKET_KEY).cloned() else {
            return reject(
                StatusCode::BAD_REQUEST,
                "`Sec-WebSocket-Key` header missing",
            );
        };
        if !header_eq(headers, header::SEC_WEBSOCKET_VERSION, "13") {
            return reject(
                StatusCode::BAD_REQUEST,
                "`Sec-WebSocket-Version` header did not include '13'",
            );
        }
        let Some(on_upgrade) = request.extensions_mut().remove::<OnUpgrade>() else {
            return reject(
                StatusCode::UPGRADE_REQUIRED,
                "WebSocket request couldn't be upgraded since no upgrade state was present",
            );
        };
        Ok(Self {
            key,
            on_upgrade,
            protocol: None,
            config: WebSocketConfig::default().read_buffer_size(READ_BUFFER_SIZE),
        })
    }

    /// The subprotocol the server picked out of the ones of the client.
    pub fn protocol(mut self, protocol: Option<&'static str>) -> Self {
        self.protocol = protocol;
        self
    }

    /// The largest message and frame the client may send.
    pub fn max_message_size(mut self, max: usize) -> Self {
        self.config = self
            .config
            .max_message_size(Some(max))
            .max_frame_size(Some(max));
        self
    }

    /// Answer the request, the WebSocket goes to `callback` once the
    /// connection got upgraded.
    pub fn on_upgrade<C, F>(self, callback: C) -> Response
    where
        C: FnOnce(WebSocket) -> F + Send + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let Self {
            key,
            on_upgrade,
            protocol,
            config,
        } = self;
        tokio::spawn(async move {
            let upgraded = match on_upgrade.await {
                Ok(upgraded) => upgraded,
                Err(e) => {
                    debug!("Unable to upgrade connection to a WebSocket: {e}");
                    return;
                }
            };
            let socket = WebSocketStream::from_raw_socket(
                ClientIo::new(TokioIo::new(upgraded)),
                Role::Server,
                Some(config),
            )
            .await;
            callback(socket).await;
        });

        let mut response = status(StatusCode::SWITCHING_PROTOCOLS);
        let headers = response.headers_mut();
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        if let Ok(accept) = HeaderValue::from_str(&derive_accept_key(key.as_bytes())) {
            headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
        }
        if let Some(protocol) = protocol {
            headers.insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_static(protocol),
            );
        }
        response
    }
}

/// Whether one of the values of the header is `value`, ignoring the case.
fn header_eq(headers: &HeaderMap, name: HeaderName, value: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .any(|v| v.as_bytes().eq_ignore_ascii_case(value.as_bytes()))
}

/// The comma separated tokens of all values of the header, without the
/// empty ones, like `keep-alive` and `Upgrade` of
/// `Connection: keep-alive, Upgrade`.
pub(crate) fn header_tokens(headers: &HeaderMap, name: HeaderName) -> impl Iterator<Item = &str> {
    headers
        .get_all(name)
        .into_iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Whether one of the tokens of the header is `token`, ignoring the case,
/// see [`header_tokens`].
pub(crate) fn header_has_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    header_tokens(headers, name).any(|t| t.eq_ignore_ascii_case(token))
}

#[cfg(test)]
mod tests;
