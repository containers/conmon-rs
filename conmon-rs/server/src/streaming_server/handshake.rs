//! The upgrade of a request to a WebSocket or to SPDY, by the protocols the
//! client asks for.

use super::{MAX_MESSAGE_SIZE, Session, State, StreamingServer};
use crate::{
    random_id::RandomId,
    streaming_server::{
        port_forward::{self, PortForward, WebSocketProtocol},
        remotecommand::{Framing, SPDY_PROTOCOLS, Version, spdy_version},
        web::{self, Response, WebSocketUpgrade},
    },
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
    /// Handle an exec or attach request, `kind` is the path of its URL, which
    /// the client upgrades to a WebSocket or SPDY.
    pub(super) async fn handle(
        kind: &str,
        token: RandomId,
        state: Arc<RwLock<State>>,
        mut request: Request<Incoming>,
    ) -> Response {
        async move {
            debug!("Got request");
            // An unknown token comes first, the request cannot succeed. The
            // token of a session of the other kind is unknown on this path,
            // the session stays for its own.
            let typ = state.read().await.sessions.get(&token).map(Session::typ);
            if typ != Some(kind) {
                debug!("Unable to find session");
                return web::status(StatusCode::NOT_FOUND);
            }
            if Self::is_websocket_request(request.headers()) {
                let requested = Self::websocket_protocols(request.headers());
                let Some((framing, subprotocol)) =
                    Framing::negotiate(requested.iter().map(String::as_str))
                else {
                    return Self::unsupported_protocols(&requested);
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

    /// The answer to a WebSocket upgrade without a supported subprotocol.
    fn unsupported_protocols(requested: &[String]) -> Response {
        web::text(
            StatusCode::FORBIDDEN,
            format!("unable to upgrade: requested protocol(s) are not supported: {requested:?}"),
        )
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
            return Self::unsupported_protocols(&requested);
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
pub mod fuzzing;

#[cfg(test)]
mod tests;
