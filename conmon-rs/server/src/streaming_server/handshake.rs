//! The upgrade of a request to a WebSocket or to SPDY, by the protocols the
//! client asks for.

use super::{MAX_MESSAGE_SIZE, State, StreamingServer};
use crate::{
    port_forward::{self, PortForward, WebSocketProtocol},
    random_id::RandomId,
    remotecommand::{Framing, SPDY_PROTOCOLS, Version, spdy_version},
    web::{self, Response, WebSocketUpgrade},
};
use anyhow::Result;
use hyper::{
    HeaderMap, Request, StatusCode,
    body::Incoming,
    header::{self, HeaderName, HeaderValue},
    upgrade::{OnUpgrade, Upgraded},
};
use hyper_util::rt::TokioIo;
use std::{future::Future, sync::Arc};
use tokio::{sync::RwLock, task};
use tracing::{Instrument, Span, debug, debug_span};

/// The upgrade port forward clients ask for.
const SPDY_31: &str = "SPDY/3.1";

/// The streaming protocols of a SPDY upgrade request, and the one the server
/// picked in the response.
const PROTOCOL_VERSION_HEADER: HeaderName = HeaderName::from_static("x-stream-protocol-version");

/// The protocols the server supports, if it supports none of the client.
const ACCEPTED_PROTOCOL_VERSIONS_HEADER: HeaderName =
    HeaderName::from_static("x-accepted-stream-protocol-versions");

impl StreamingServer {
    /// Handle an exec or attach request, which the client upgrades to a
    /// WebSocket or SPDY.
    pub(super) async fn handle(
        token: RandomId,
        state: Arc<RwLock<State>>,
        mut request: Request<Incoming>,
    ) -> Response {
        async move {
            debug!("Got request");
            // An unknown token comes first, the request cannot succeed.
            if !state.read().await.sessions.contains_key(&token) {
                debug!("Unable to find session");
                return web::status(StatusCode::NOT_FOUND);
            }
            if Self::is_websocket_request(request.headers()) {
                let requested = Self::websocket_protocols(request.headers());
                let Some((framing, subprotocol)) =
                    Framing::negotiate(requested.iter().map(String::as_str))
                else {
                    return web::text(
                        StatusCode::FORBIDDEN,
                        format!(
                            "unable to upgrade: requested protocol(s) are not supported: {requested:?}"
                        ),
                    );
                };
                let ws = match WebSocketUpgrade::from_request(&mut request) {
                    Ok(ws) => ws,
                    Err(rejection) => return *rejection,
                };
                let Some(session) = Self::take_session(&state, token).await else {
                    return web::status(StatusCode::NOT_FOUND);
                };
                return ws
                    .protocol(subprotocol)
                    .max_message_size(MAX_MESSAGE_SIZE)
                    .on_upgrade(move |socket| {
                        Self::handle_websocket(socket, session, framing)
                            .instrument(debug_span!("websocket", ?token))
                    });
            }

            let supported = SPDY_PROTOCOLS.map(|(protocol, _)| protocol);
            Self::upgrade_spdy(
                request,
                &supported,
                Self::take_session(&state, token),
                |io, session, protocol| {
                    let version = spdy_version(protocol).unwrap_or(Version::V1);
                    Self::handle_spdy(io, session, version)
                },
                debug_span!("spdy", ?token),
            )
            .await
        }
        .instrument(debug_span!("handle_common", ?token))
        .await
    }

    /// Answer a SPDY upgrade request with one of the supported protocols and
    /// serve the connection with the session once it got upgraded. The
    /// session gets taken only for a valid request.
    async fn upgrade_spdy<S, F>(
        mut request: Request<Incoming>,
        supported: &[&'static str],
        take_session: impl Future<Output = Option<S>>,
        serve: impl FnOnce(TokioIo<Upgraded>, S, &'static str) -> F + Send + 'static,
        span: Span,
    ) -> Response
    where
        S: Send + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let protocol = match Self::spdy_handshake(request.headers(), supported) {
            Ok(protocol) => protocol,
            Err(response) => return *response,
        };
        let Some(on_upgrade) = request.extensions_mut().remove::<OnUpgrade>() else {
            return web::text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unable to upgrade: connection does not support upgrades",
            );
        };
        let Some(session) = take_session.await else {
            return web::status(StatusCode::NOT_FOUND);
        };
        task::spawn(
            async move {
                match on_upgrade.await {
                    Ok(upgraded) => serve(TokioIo::new(upgraded), session, protocol).await,
                    Err(e) => debug!("Unable to upgrade connection: {e}"),
                }
            }
            .instrument(span),
        );
        Self::switching_protocols(protocol)
    }

    /// The response of a successful SPDY upgrade.
    fn switching_protocols(protocol: &'static str) -> Response {
        let mut response = web::status(StatusCode::SWITCHING_PROTOCOLS);
        let headers = response.headers_mut();
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static(SPDY_31));
        headers.insert(PROTOCOL_VERSION_HEADER, HeaderValue::from_static(protocol));
        response
    }

    /// The WebSocket subprotocols a client requested.
    fn websocket_protocols(headers: &HeaderMap) -> Vec<String> {
        headers
            .get_all(header::SEC_WEBSOCKET_PROTOCOL)
            .into_iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// Handle a port forward request, which the client upgrades to SPDY.
    pub(super) async fn handle_port_forward(
        token: RandomId,
        state: Arc<RwLock<State>>,
        request: Request<Incoming>,
    ) -> Response {
        async move {
            debug!("Got port forward request");
            // An unknown token comes first, the request cannot succeed.
            if !state.read().await.port_forwards.contains_key(&token) {
                debug!("Unable to find port forward session");
                return web::status(StatusCode::NOT_FOUND);
            }
            if Self::is_websocket_request(request.headers()) {
                return Self::upgrade_port_forward_websocket(token, &state, request).await;
            }
            Self::upgrade_spdy(
                request,
                &[port_forward::PROTOCOL],
                Self::take_port_forward(&state, token),
                |io, session: PortForward, _| session.serve_spdy(io),
                debug_span!("port_forward", ?token),
            )
            .await
        }
        .instrument(debug_span!("handle_port_forward", ?token))
        .await
    }

    fn is_websocket_request(headers: &HeaderMap) -> bool {
        web::header_has_token(headers, header::UPGRADE, "websocket")
    }

    /// Upgrade a port forward request to a WebSocket, which tunnels SPDY or
    /// speaks the channel protocol, by the subprotocols of the client.
    async fn upgrade_port_forward_websocket(
        token: RandomId,
        state: &RwLock<State>,
        mut request: Request<Incoming>,
    ) -> Response {
        let requested = Self::websocket_protocols(request.headers());
        let Some((protocol, subprotocol)) =
            WebSocketProtocol::negotiate(requested.iter().map(String::as_str))
        else {
            return web::text(
                StatusCode::FORBIDDEN,
                format!(
                    "unable to upgrade: requested protocol(s) are not supported: {requested:?}"
                ),
            );
        };
        let ports = match protocol {
            WebSocketProtocol::SpdyTunnel => vec![],
            WebSocketProtocol::Channels { .. } => {
                match PortForward::query_ports(request.uri().query()) {
                    Ok(ports) => ports,
                    Err(e) => return web::text(StatusCode::BAD_REQUEST, format!("{e:#}")),
                }
            }
        };

        let ws = match WebSocketUpgrade::from_request(&mut request) {
            Ok(ws) => ws,
            Err(rejection) => return *rejection,
        };
        let Some(session) = Self::take_port_forward(state, token).await else {
            return web::status(StatusCode::NOT_FOUND);
        };
        ws.protocol(subprotocol)
            .max_message_size(MAX_MESSAGE_SIZE)
            .on_upgrade(move |socket| {
                session
                    .serve_websocket(socket, protocol, ports)
                    .instrument(debug_span!("port_forward", ?token))
            })
    }

    /// Check that the request asks for a SPDY/3.1 upgrade and negotiate one
    /// of the supported protocols, by the order of the client. The rejections
    /// are the ones of the SPDY server of Kubernetes.
    fn spdy_handshake(
        headers: &HeaderMap,
        supported: &[&'static str],
    ) -> Result<&'static str, Box<Response>> {
        let values = |name| {
            headers
                .get_all(name)
                .into_iter()
                .filter_map(|v| v.to_str().ok())
        };
        if !web::header_has_token(headers, header::CONNECTION, "upgrade")
            || !web::header_has_token(headers, header::UPGRADE, SPDY_31)
        {
            return Err(Box::new(web::text(
                StatusCode::BAD_REQUEST,
                "unable to upgrade: missing WebSocket or SPDY/3.1 upgrade headers in request",
            )));
        }

        let protocols: Vec<_> = values(PROTOCOL_VERSION_HEADER)
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if protocols.is_empty() {
            return Err(Box::new(web::text(
                StatusCode::BAD_REQUEST,
                format!("unable to upgrade: header {PROTOCOL_VERSION_HEADER} is required"),
            )));
        }
        protocols
            .iter()
            .find_map(|protocol| supported.iter().find(|s| *s == protocol).copied())
            .ok_or_else(|| {
                let mut response = web::text(
                    StatusCode::FORBIDDEN,
                    format!(
                        "unable to upgrade: unable to negotiate protocol: client supports {protocols:?}, server accepts {supported:?}"
                    ),
                );
                for protocol in supported {
                    response.headers_mut().append(
                        ACCEPTED_PROTOCOL_VERSIONS_HEADER,
                        HeaderValue::from_static(protocol),
                    );
                }
                Box::new(response)
            })
    }
}

/// The entry points of the fuzz targets.
#[cfg(feature = "fuzzing")]
pub mod fuzzing {
    use super::*;
    use hyper::Method;

    /// Check the headers of an upgrade request, one `name: value` per line
    /// of the input, like the server does for a WebSocket or SPDY. Lines
    /// which are no valid header get skipped.
    pub fn upgrade_headers(input: &[u8]) {
        let mut request = Request::new(());
        if input.first().is_some_and(|b| b & 1 == 1) {
            *request.method_mut() = Method::POST;
        }
        let headers = request.headers_mut();
        for line in input.split(|b| *b == b'\n') {
            let Some(colon) = line.iter().position(|b| *b == b':') else {
                continue;
            };
            let (name, value) = (&line[..colon], line[colon + 1..].trim_ascii());
            if let (Ok(name), Ok(value)) =
                (HeaderName::from_bytes(name), HeaderValue::from_bytes(value))
            {
                headers.append(name, value);
            }
        }

        let supported = SPDY_PROTOCOLS.map(|(protocol, _)| protocol);
        if let Ok(protocol) = StreamingServer::spdy_handshake(request.headers(), &supported) {
            assert!(supported.contains(&protocol));
        }
        let _ = StreamingServer::spdy_handshake(request.headers(), &[port_forward::PROTOCOL]);
        let requested = StreamingServer::websocket_protocols(request.headers());
        let _ = Framing::negotiate(requested.iter().map(String::as_str));
        let _ = WebSocketProtocol::negotiate(requested.iter().map(String::as_str));
        let _ = StreamingServer::is_websocket_request(request.headers());
        // Without a connection to upgrade, no request passes.
        assert!(WebSocketUpgrade::from_request(&mut request).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        child::{Child, ChildKind},
        child_reaper::ReapableChild,
        container_io::{ContainerIO, SharedContainerIO},
        container_log::ContainerLog,
        port_forward,
        random_id::RandomId,
        remotecommand::SPDY_PROTOCOLS,
        spdy::testing::{Client, Frame, Headers},
        streaming_server::{
            ADDR, ATTACH_PATH, AttachSession, PORT_FORWARD_PATH, StreamingServer, testing::*,
        },
    };
    use anyhow::{Context, Result};

    use data_encoding::BASE64;
    use hyper::{
        HeaderMap, StatusCode,
        header::{self, HeaderValue},
    };
    use nix::unistd::Pid;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        task::LocalSet,
        time::{self, Duration},
    };
    use tokio_util::sync::CancellationToken;

    #[test]
    fn upgrade_headers_count_in_all_values() {
        let mut headers = HeaderMap::new();
        headers.append(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.append(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.append(header::UPGRADE, HeaderValue::from_static("spdy/3.1"));
        headers.append(
            PROTOCOL_VERSION_HEADER,
            HeaderValue::from_static("v5.channel.k8s.io"),
        );
        headers.append(
            PROTOCOL_VERSION_HEADER,
            HeaderValue::from_static("v4.channel.k8s.io"),
        );
        assert!(matches!(
            StreamingServer::spdy_handshake(&headers, &["v4.channel.k8s.io"]),
            Ok("v4.channel.k8s.io")
        ));
        assert!(!StreamingServer::is_websocket_request(&headers));

        headers.append(header::UPGRADE, HeaderValue::from_static("h2c, WebSocket"));
        assert!(StreamingServer::is_websocket_request(&headers));

        // A token which only contains the protocol is none.
        let mut headers = HeaderMap::new();
        headers.append(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.append(header::UPGRADE, HeaderValue::from_static("SPDY/3.10"));
        let Err(response) = StreamingServer::spdy_handshake(&headers, &["v4.channel.k8s.io"])
        else {
            panic!("upgraded to SPDY/3.10");
        };
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// Send a port forward request with the extra headers, return the
    /// response head and the connection.
    async fn port_forward_request(
        server: &StreamingServer,
        url: &str,
        headers: &str,
    ) -> Result<(String, TcpStream)> {
        let path = url
            .strip_prefix(&format!("http://{ADDR}:{}", server.port))
            .context("url")?;
        let mut stream = TcpStream::connect((ADDR, server.port)).await?;
        stream
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: {ADDR}\r\nContent-Length: 0\r\n{headers}\r\n"
                )
                .as_bytes(),
            )
            .await?;
        Ok((response_head(&mut stream).await?, stream))
    }

    const SPDY_UPGRADE: &str = "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n";

    /// A valid SPDY port forward upgrade.
    const SPDY_PORT_FORWARD: &str = "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                                     X-Stream-Protocol-Version: portforward.k8s.io\r\n";

    #[tokio::test]
    async fn port_forward_upgrades_to_spdy() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                let url = server
                    .port_forward_url("/proc/self/ns/net".into(), None)
                    .await?;

                let (response, stream) = port_forward_request(
                    &server,
                    &url,
                    &format!("{SPDY_UPGRADE}X-Stream-Protocol-Version: v4.channel.k8s.io, portforward.k8s.io\r\n"),
                )
                .await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                assert!(response.contains("upgrade: spdy/3.1"), "{response}");
                assert!(
                    response.contains("x-stream-protocol-version: portforward.k8s.io"),
                    "{response}"
                );

                // The connection speaks SPDY now, a stream without headers
                // gets rejected.
                let mut client = Client::new(stream)?;
                client.open(1, Headers::default()).await?;
                assert!(matches!(
                    client.frame().await?,
                    Some(Frame::RstStream { stream_id: 1, .. })
                ));

                // The token got used, also a request with bad headers learns
                // that first.
                let (response, _) = port_forward_request(&server, &url, SPDY_UPGRADE).await?;
                assert!(response.starts_with("http/1.1 404"), "{response}");
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_tokens_survive_rejected_requests() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;

                // A SPDY request without the protocol, then a valid one.
                let url = server
                    .port_forward_url("/proc/self/ns/net".into(), None)
                    .await?;
                let (response, _) = port_forward_request(&server, &url, SPDY_UPGRADE).await?;
                assert!(response.starts_with("http/1.1 400"), "{response}");
                let (response, _) = port_forward_request(&server, &url, SPDY_PORT_FORWARD).await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");

                // A WebSocket request without ports, then a valid one.
                let url = server
                    .port_forward_url("/proc/self/ns/net".into(), None)
                    .await?;
                let (response, _) = port_forward_websocket_to(&server, &url, "", "").await?;
                assert!(response.starts_with("http/1.1 400"), "{response}");
                let (response, _) =
                    port_forward_websocket_to(&server, &url, "?port=80", "").await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_rejects_other_upgrades() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;

                for (headers, status) in [
                    (
                        "Connection: Upgrade\r\nUpgrade: websocket\r\n\
                         X-Stream-Protocol-Version: portforward.k8s.io\r\n"
                            .to_owned(),
                        "400",
                    ),
                    (SPDY_UPGRADE.to_owned(), "400"),
                    (
                        format!("{SPDY_UPGRADE}X-Stream-Protocol-Version: v4.channel.k8s.io\r\n"),
                        "403",
                    ),
                ] {
                    let url = server
                        .port_forward_url("/proc/self/ns/net".into(), None)
                        .await?;
                    let (response, _) = port_forward_request(&server, &url, &headers).await?;
                    assert!(
                        response.starts_with(&format!("http/1.1 {status}")),
                        "{response}"
                    );
                    if status == "403" {
                        assert!(
                            response.contains(
                                "x-accepted-stream-protocol-versions: portforward.k8s.io"
                            ),
                            "{response}"
                        );
                    }
                }

                // Exec and attach tokens are no port forward tokens.
                let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
                let (_, child) = ReapableChild::from_child(Child {
                    id: "id".into(),
                    kind: ChildKind::Container,
                    pid: Pid::from_raw(0),
                    exit_paths: vec![],
                    oom_exit_paths: vec![],
                    timeout: None,
                    io,
                    cleanup_cmd: vec![],
                    token: CancellationToken::new(),
                });
                let url = server
                    .attach_url(AttachSession {
                        container_id: "id".into(),
                        child,
                        stdin: false,
                        stdout: true,
                        stderr: false,
                        stop_after_stdin_eof: false,
                        idle_timeout: None,
                    })
                    .await?
                    .replace(ATTACH_PATH, PORT_FORWARD_PATH);
                let (response, _) = port_forward_request(&server, &url, SPDY_PORT_FORWARD).await?;
                assert!(response.starts_with("http/1.1 404"), "{response}");
                Ok(())
            })
            .await
    }

    /// Upgrade a port forward request to a WebSocket with the subprotocols
    /// and the query, return the response head and the connection.
    async fn port_forward_websocket(
        server: &StreamingServer,
        query: &str,
        protocols: &str,
    ) -> Result<(String, TcpStream)> {
        port_forward_websocket_with(server, query, protocols, None).await
    }

    async fn port_forward_websocket_with(
        server: &StreamingServer,
        query: &str,
        protocols: &str,
        idle_timeout: Option<Duration>,
    ) -> Result<(String, TcpStream)> {
        let url = server
            .port_forward_url("/proc/self/ns/net".into(), idle_timeout)
            .await?;
        port_forward_websocket_to(server, &url, query, protocols).await
    }

    /// Upgrade a request for the URL to a WebSocket.
    async fn port_forward_websocket_to(
        server: &StreamingServer,
        url: &str,
        query: &str,
        protocols: &str,
    ) -> Result<(String, TcpStream)> {
        let path = url
            .strip_prefix(&format!("http://{ADDR}:{}", server.port))
            .context("url")?;
        let mut stream = TcpStream::connect((ADDR, server.port)).await?;
        let protocols = if protocols.is_empty() {
            String::new()
        } else {
            format!("Sec-WebSocket-Protocol: {protocols}\r\n")
        };
        stream
            .write_all(
                format!(
                    "GET {path}{query} HTTP/1.1\r\nHost: {ADDR}\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{protocols}\r\n"
                )
                .as_bytes(),
            )
            .await?;
        Ok((response_head(&mut stream).await?, stream))
    }

    #[tokio::test]
    async fn port_forward_over_websocket_channels() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;

                for base64 in [false, true] {
                    // Two ports which listen and one between them which
                    // does not.
                    let first = tokio::net::TcpListener::bind((ADDR, 0)).await?;
                    let (_closed, closed) = port_forward::testing::closed_port()?;
                    let last = tokio::net::TcpListener::bind((ADDR, 0)).await?;
                    let ports = [
                        first.local_addr()?.port(),
                        closed,
                        last.local_addr()?.port(),
                    ];
                    let protocol = if base64 {
                        "v4.base64.channel.k8s.io"
                    } else {
                        "v4.channel.k8s.io"
                    };
                    let (response, mut stream) = port_forward_websocket(
                        &server,
                        &format!("?port={},{}&port={}", ports[0], ports[1], ports[2]),
                        protocol,
                    )
                    .await?;
                    assert!(response.starts_with("http/1.1 101"), "{response}");
                    assert!(
                        response.contains(&format!("sec-websocket-protocol: {protocol}")),
                        "{response}"
                    );

                    // A message of the channel protocol, text for base64.
                    let message = |channel: u8, data: &[u8]| {
                        if base64 {
                            let mut text = vec![b'0' + channel];
                            text.extend(BASE64.encode(data).into_bytes());
                            (1, text)
                        } else {
                            (2, [&[channel], data].concat())
                        }
                    };
                    // The channel of a message and its data.
                    let decode = |(opcode, payload): (u8, Vec<u8>)| -> Result<(u8, Vec<u8>)> {
                        let (channel, data) = payload.split_first().context("empty message")?;
                        Ok(if base64 {
                            assert_eq!(opcode, 1);
                            (channel - b'0', BASE64.decode(data)?)
                        } else {
                            assert_eq!(opcode, 2);
                            (*channel, data.to_vec())
                        })
                    };

                    // Port i gets the data channel 2i and the error channel
                    // 2i+1, each starts with the port. The port which does
                    // not listen reports its failure on its error channel,
                    // which may come before the ports after it.
                    let mut messages = vec![];
                    for _ in 0..7 {
                        messages.push(decode(server_frame(&mut stream).await?)?);
                    }
                    let error = messages
                        .iter()
                        .position(|(channel, data)| *channel == 3 && data.len() > 2)
                        .context("no error of the closed port")?;
                    let (_, error) = messages.remove(error);
                    let error = String::from_utf8(error)?;
                    assert!(
                        error.starts_with(&format!("error forwarding port {closed}:")),
                        "{error}"
                    );
                    let expected: Vec<_> = (0..6)
                        .map(|channel| {
                            (
                                channel,
                                ports[usize::from(channel / 2)].to_le_bytes().to_vec(),
                            )
                        })
                        .collect();
                    assert_eq!(messages, expected);

                    // Each data channel goes to its own port.
                    for (channel, listener) in [(0, &first), (4, &last)] {
                        let (opcode, payload) = message(channel, &[b'0' + channel]);
                        stream.write_all(&client_frame(opcode, &payload)).await?;
                        let (mut connection, _) = listener.accept().await?;
                        let mut request = [0];
                        connection.read_exact(&mut request).await?;
                        assert_eq!(request, [b'0' + channel]);
                        connection.write_all(b"response").await?;
                        drop(connection);
                        assert_eq!(
                            server_frame(&mut stream).await?,
                            message(channel, b"response")
                        );
                    }

                    // The last port is done, which ends the session.
                    assert_eq!(server_frame(&mut stream).await?.0, 8);
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_ends_idle_sessions_with_stuck_ports() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                // A pod which accepts, but never reads.
                let listener = tokio::net::TcpListener::bind((ADDR, 0)).await?;
                let port = listener.local_addr()?.port();
                let accepted = tokio::spawn(async move { listener.accept().await });

                let (response, stream) = port_forward_websocket_with(
                    &server,
                    &format!("?port={port}"),
                    "",
                    Some(Duration::from_millis(500)),
                )
                .await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                let (mut reader, mut writer) = stream.into_split();
                server_frame(&mut reader).await?;
                server_frame(&mut reader).await?;
                let _connection = accepted.await??;

                // More than the socket buffers and the port queue hold.
                let flood = tokio::spawn(async move {
                    let frame = client_frame(2, &[&[0], &[0; 60_000][..]].concat());
                    while writer.write_all(&frame).await.is_ok() {}
                });
                let (opcode, _) = time::timeout(Duration::from_secs(10), server_frame(&mut reader))
                    .await
                    .context("no close of the idle session")??;
                assert_eq!(opcode, 8);
                flood.abort();
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_ends_with_its_forwards() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                // A pod which closes the connection right away.
                let listener = tokio::net::TcpListener::bind((ADDR, 0)).await?;
                let port = listener.local_addr()?.port();
                let accepted = tokio::spawn(async move {
                    let (connection, _) = listener.accept().await?;
                    drop(connection);
                    anyhow::Ok(())
                });

                let (response, stream) =
                    port_forward_websocket_with(&server, &format!("?port={port}"), "", None)
                        .await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                let (mut reader, mut writer) = stream.into_split();
                server_frame(&mut reader).await?;
                server_frame(&mut reader).await?;
                accepted.await??;

                // The client keeps sending, the session still ends with the
                // forward of its only port.
                let flood = tokio::spawn(async move {
                    let frame = client_frame(2, &[0, b'x']);
                    while writer.write_all(&frame).await.is_ok() {}
                });
                time::timeout(Duration::from_secs(10), async {
                    loop {
                        let (opcode, _) = server_frame(&mut reader).await?;
                        if opcode == 8 {
                            return anyhow::Ok(());
                        }
                    }
                })
                .await
                .context("no close after the forward ended")??;
                flood.abort();
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_waits_for_the_close_answer() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                let (_socket, port) = port_forward::testing::closed_port()?;
                let (response, mut stream) =
                    port_forward_websocket(&server, &format!("?port={port}"), "").await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                server_frame(&mut stream).await?;
                server_frame(&mut stream).await?;

                // The port fails before the client sent anything, the error
                // still reaches it.
                let (opcode, error) = server_frame(&mut stream).await?;
                assert_eq!((opcode, error[0]), (2, 1));
                let error = String::from_utf8_lossy(&error[1..]);
                assert!(error.contains("error forwarding port"), "{error}");
                assert_eq!(server_frame(&mut stream).await?.0, 8);
                answer_close(&mut stream).await
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_pings_do_not_keep_sessions_active() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                let listener = tokio::net::TcpListener::bind((ADDR, 0)).await?;
                let port = listener.local_addr()?.port();
                let accepted = tokio::spawn(async move { listener.accept().await });

                let (response, stream) = port_forward_websocket_with(
                    &server,
                    &format!("?port={port}"),
                    "",
                    Some(Duration::from_millis(500)),
                )
                .await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                let (mut reader, mut writer) = stream.into_split();
                server_frame(&mut reader).await?;
                server_frame(&mut reader).await?;
                let _connection = accepted.await??;

                // A heartbeat like the one of client-go.
                let heartbeat = tokio::spawn(async move {
                    for opcode in [0x9, 0xa].into_iter().cycle() {
                        time::sleep(Duration::from_millis(100)).await;
                        if writer.write_all(&client_frame(opcode, &[])).await.is_err() {
                            return;
                        }
                    }
                });
                time::timeout(Duration::from_secs(10), async {
                    loop {
                        // The pongs which answer the pings come first.
                        let (opcode, _) = server_frame(&mut reader).await?;
                        if opcode == 8 {
                            return anyhow::Ok(());
                        }
                        assert_eq!(opcode, 0xa);
                    }
                })
                .await
                .context("no close of the idle session")??;
                heartbeat.abort();
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_reports_errors() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                let (_socket, port) = port_forward::testing::closed_port()?;

                let (response, mut stream) =
                    port_forward_websocket(&server, &format!("?port={port}"), "").await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                server_frame(&mut stream).await?;
                server_frame(&mut stream).await?;

                let (opcode, payload) = server_frame(&mut stream).await?;
                assert_eq!((opcode, payload[0]), (2, 1));
                let error = String::from_utf8_lossy(&payload[1..]).into_owned();
                assert!(
                    error.starts_with(&format!(
                        "error forwarding port {port}: failed to connect to localhost:{port}"
                    )),
                    "{error}"
                );
                assert_eq!(server_frame(&mut stream).await?.0, 8);
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_rejects_invalid_requests() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                for (query, protocols, status) in [
                    ("", "v4.channel.k8s.io", "400"),
                    ("?port=0", "", "400"),
                    ("?port=80", "v5.channel.k8s.io", "403"),
                ] {
                    let (response, _) = port_forward_websocket(&server, query, protocols).await?;
                    assert!(
                        response.starts_with(&format!("http/1.1 {status}")),
                        "{response}"
                    );
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn port_forward_over_websocket_tunnels_spdy() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                // The tunnel needs no ports, they are in the SPDY streams.
                let (response, mut stream) = port_forward_websocket(
                    &server,
                    "",
                    "v4.channel.k8s.io, SPDY/3.1+portforward.k8s.io",
                )
                .await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                assert!(
                    response.contains("sec-websocket-protocol: spdy/3.1+portforward.k8s.io"),
                    "{response}"
                );

                // A stream without headers, which gets rejected.
                let (mut client, mut encoded) = Client::pair()?;
                client.open(1, Headers::default()).await?;
                let mut syn_stream = vec![0; 1024];
                let len = encoded.read(&mut syn_stream).await?;
                stream
                    .write_all(&client_frame(2, &syn_stream[..len]))
                    .await?;

                let (opcode, payload) = server_frame(&mut stream).await?;
                assert_eq!(opcode, 2);
                assert_eq!(
                    payload,
                    [
                        0x80, 0x03, 0, 0x03, 0, 0, 0, 0x08, 0, 0, 0, 0x01, 0, 0, 0, 0x05
                    ]
                );
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn spdy_upgrades_get_negotiated() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                for (headers, status) in [
                    ("Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n", "400"),
                    (
                        "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                         X-Stream-Protocol-Version: v5.channel.k8s.io\r\n",
                        "403",
                    ),
                    ("", "400"),
                ] {
                    let (_, child, _token) = attach_child(false)?;
                    let url = server
                        .attach_url(AttachSession {
                            container_id: "id".into(),
                            child,
                            stdin: false,
                            stdout: true,
                            stderr: false,
                            stop_after_stdin_eof: false,
                            idle_timeout: None,
                        })
                        .await?;
                    let (response, _) = upgrade(&server, &url, "POST", headers).await?;
                    assert!(
                        response.starts_with(&format!("http/1.1 {status}")),
                        "{response}"
                    );
                    if status == "403" {
                        for protocol in SPDY_PROTOCOLS.map(|(p, _)| p) {
                            assert!(
                                response.contains(&format!(
                                    "x-accepted-stream-protocol-versions: {protocol}"
                                )),
                                "{response}"
                            );
                        }
                    }
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn rejected_upgrades_keep_the_token() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                let websocket = |protocol: &str| {
                    format!(
                        "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: {protocol}\r\n"
                    )
                };
                // An unknown token comes before the protocol.
                let (_, child, _token) = attach_child(false)?;
                let url = server
                    .attach_url(AttachSession {
                        container_id: "id".into(),
                        child,
                        stdin: false,
                        stdout: true,
                        stderr: false,
                        stop_after_stdin_eof: false,
                        idle_timeout: None,
                    })
                    .await?;
                let unknown = url.replace(
                    url.rsplit('/').next().context("token")?,
                    &RandomId::new().to_string(),
                );
                let (response, _) = upgrade(&server, &unknown, "POST", "").await?;
                assert!(response.starts_with("http/1.1 404"), "{response}");

                for (rejected, status, valid) in [
                    (
                        "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                         X-Stream-Protocol-Version: v5.channel.k8s.io\r\n"
                            .to_owned(),
                        "403",
                        SPDY_EXEC_UPGRADE.to_owned(),
                    ),
                    ("".to_owned(), "400", SPDY_EXEC_UPGRADE.to_owned()),
                    (
                        websocket("v3.channel.k8s.io"),
                        "403",
                        websocket(PROTOCOL_V5),
                    ),
                ] {
                    let (_, child, _token) = attach_child(false)?;
                    let url = server
                        .attach_url(AttachSession {
                            container_id: "id".into(),
                            child,
                            stdin: false,
                            stdout: true,
                            stderr: false,
                            stop_after_stdin_eof: false,
                            idle_timeout: None,
                        })
                        .await?;
                    let (response, _) = upgrade(&server, &url, "POST", &rejected).await?;
                    assert!(response.starts_with(&format!("http/1.1 {status}")), "{response}");

                    // The client can retry with a request the server takes,
                    // which uses the token.
                    let method = if valid.contains("websocket") { "GET" } else { "POST" };
                    let (response, _) = upgrade(&server, &url, method, &valid).await?;
                    assert!(response.starts_with("http/1.1 101"), "{response}");
                    let (response, _) = upgrade(&server, &url, method, &valid).await?;
                    assert!(response.starts_with("http/1.1 404"), "{response}");
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn unsupported_websocket_protocols_get_rejected() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (_, _server, _, _, response) = websocket_attach("v3.channel.k8s.io").await?;
                assert!(response.starts_with("http/1.1 403"), "{response}");
                Ok(())
            })
            .await
    }
}
