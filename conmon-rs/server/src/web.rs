//! The HTTP side of the streaming server on top of hyper: short responses
//! and the upgrade of a request to a WebSocket. The WebSocket handshake
//! checks the request like RFC 6455 says, a request which does not pass gets
//! a plain text response which tells what is missing.

use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
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
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    task::JoinHandle,
    time::{self, Duration},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        handshake::derive_accept_key,
        protocol::{Role, WebSocketConfig},
    },
};
use tracing::debug;

pub use tokio_tungstenite::tungstenite::{
    Message,
    protocol::{CloseFrame, frame::coding::CloseCode},
};

/// A WebSocket of a client, after the upgrade of its request.
pub type WebSocket = WebSocketStream<TokioIo<Upgraded>>;

/// How long a client gets to take the end of a WebSocket, the close of the
/// server and what is queued before it.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The close of the server once it is done.
pub fn close_message(reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code: CloseCode::Normal,
        reason: reason.into(),
    }))
}

/// Wait for the task which writes to a WebSocket and closes it, abort it
/// after [`CLOSE_TIMEOUT`]. Dropping its handle would leave it running, a
/// writer to a client which does not read would hold the connection. The
/// output of the writer, `None` if it did not end in time.
pub async fn join_or_abort<T>(mut writer: JoinHandle<T>) -> Option<T> {
    match time::timeout(CLOSE_TIMEOUT, &mut writer).await {
        Ok(output) => output.ok(),
        Err(_) => {
            writer.abort();
            None
        }
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
/// [`await_close_answer`]. The writer tells whether its close went out, a
/// client gets no time to answer a close it did not get.
pub async fn finish<S, E>(writer: JoinHandle<bool>, messages: &mut S)
where
    S: Stream<Item = Result<Message, E>> + Unpin,
{
    if join_or_abort(writer).await == Some(true) {
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
            config: WebSocketConfig::default(),
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
                TokioIo::new(upgraded),
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

/// Whether one of the comma separated tokens of all values of the header is
/// `token`, ignoring the case, like `Connection: keep-alive, Upgrade`.
pub(crate) fn header_has_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::poll_fn;

    /// The data of a body.
    async fn collect(mut body: Body) -> Vec<u8> {
        let mut data = vec![];
        while let Some(Ok(frame)) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            if let Ok(chunk) = frame.into_data() {
                data.extend_from_slice(&chunk);
            }
        }
        data
    }

    /// A request with the headers, which hyper could upgrade.
    fn request(headers: &[(&'static str, &'static str)]) -> Request<()> {
        let mut request = Request::new(());
        for (name, value) in headers {
            request.headers_mut().insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let on_upgrade = hyper::upgrade::on(Request::new(()));
        request.extensions_mut().insert(on_upgrade);
        request
    }

    const VALID: [(&str, &str); 4] = [
        ("connection", "keep-alive, Upgrade"),
        ("upgrade", "WebSocket"),
        ("sec-websocket-version", "13"),
        ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ];

    #[tokio::test]
    async fn upgrades_answer_the_key() {
        let mut request = request(&VALID);
        let Ok(upgrade) = WebSocketUpgrade::from_request(&mut request) else {
            panic!("valid request rejected");
        };
        let response = upgrade
            .protocol(Some("v5.channel.k8s.io"))
            .on_upgrade(|_| async {});
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        let headers = response.headers();
        // The example of RFC 6455.
        assert_eq!(
            headers[header::SEC_WEBSOCKET_ACCEPT],
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert_eq!(headers[header::SEC_WEBSOCKET_PROTOCOL], "v5.channel.k8s.io");
        assert_eq!(headers[header::UPGRADE], "websocket");
        assert_eq!(headers[header::CONNECTION], "upgrade");
    }

    #[tokio::test]
    async fn invalid_upgrades_get_rejected() {
        for (missing, status, message) in [
            (
                "connection",
                StatusCode::BAD_REQUEST,
                "Connection header did not include 'upgrade'",
            ),
            (
                "upgrade",
                StatusCode::BAD_REQUEST,
                "`Upgrade` header did not include 'websocket'",
            ),
            (
                "sec-websocket-version",
                StatusCode::BAD_REQUEST,
                "`Sec-WebSocket-Version` header did not include '13'",
            ),
            (
                "sec-websocket-key",
                StatusCode::BAD_REQUEST,
                "`Sec-WebSocket-Key` header missing",
            ),
        ] {
            let headers: Vec<_> = VALID.into_iter().filter(|(n, _)| *n != missing).collect();
            let Err(response) = WebSocketUpgrade::from_request(&mut request(&headers)) else {
                panic!("request without {missing} upgraded");
            };
            assert_eq!(response.status(), status, "{missing}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/plain; charset=utf-8"
            );
            assert_eq!(
                collect(response.into_body()).await,
                message.as_bytes(),
                "{missing}"
            );
        }

        let mut post = request(&VALID);
        *post.method_mut() = Method::POST;
        let Err(response) = WebSocketUpgrade::from_request(&mut post) else {
            panic!("POST upgraded");
        };
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

        let mut request = request(&VALID);
        request.extensions_mut().clear();
        let Err(response) = WebSocketUpgrade::from_request(&mut request) else {
            panic!("request without upgrade upgraded");
        };
        assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
    }

    #[test]
    fn header_tokens_are_found_in_all_values() {
        let mut headers = HeaderMap::new();
        headers.append(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.append(header::CONNECTION, HeaderValue::from_static("TE, Upgrade"));
        headers.append(header::UPGRADE, HeaderValue::from_static("h2c, WebSocket"));
        assert!(header_has_token(&headers, header::CONNECTION, "upgrade"));
        assert!(header_has_token(&headers, header::UPGRADE, "websocket"));
        assert!(!header_has_token(&headers, header::UPGRADE, "socket"));
        assert!(!header_has_token(&headers, header::CONNECTION, "close"));

        headers.append(header::SEC_WEBSOCKET_VERSION, HeaderValue::from_static("8"));
        headers.append(
            header::SEC_WEBSOCKET_VERSION,
            HeaderValue::from_static("13"),
        );
        assert!(header_eq(&headers, header::SEC_WEBSOCKET_VERSION, "13"));
        assert!(!header_eq(&headers, header::SEC_WEBSOCKET_VERSION, "1"));
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_writers_get_aborted() {
        let (alive_tx, alive_rx) = tokio::sync::oneshot::channel::<()>();
        let writer = tokio::spawn(async move {
            let _alive = alive_tx;
            std::future::pending::<()>().await
        });
        assert_eq!(join_or_abort(writer).await, None);
        // The task got dropped with its sender.
        assert!(alive_rx.await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn closes_which_did_not_go_out_get_no_answer() {
        let mut messages = futures::stream::pending::<Result<Message, Infallible>>();
        // A writer which hangs, which does not send its close.
        let start = time::Instant::now();
        finish(tokio::spawn(std::future::pending::<bool>()), &mut messages).await;
        assert_eq!(start.elapsed(), CLOSE_TIMEOUT);
        // One which did not get its close out.
        let start = time::Instant::now();
        finish(tokio::spawn(async { false }), &mut messages).await;
        assert_eq!(start.elapsed(), Duration::ZERO);

        // A close which went out gets its answer, what comes before it gets
        // dropped.
        let start = time::Instant::now();
        finish(tokio::spawn(async { true }), &mut messages).await;
        assert_eq!(start.elapsed(), CLOSE_TIMEOUT);
        let mut messages = futures::stream::iter([
            Ok::<_, Infallible>(Message::Binary(Bytes::from_static(b"late"))),
            Ok(close_message("")),
        ])
        .chain(futures::stream::pending());
        let start = time::Instant::now();
        finish(tokio::spawn(async { true }), &mut messages).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test]
    async fn bodies_are_sent_at_once() {
        assert!(status(StatusCode::NOT_FOUND).body().is_end_stream());
        let response = text(StatusCode::BAD_REQUEST, "bad");
        assert_eq!(response.body().size_hint().exact(), Some(3));
        assert_eq!(collect(response.into_body()).await, b"bad");
    }
}
