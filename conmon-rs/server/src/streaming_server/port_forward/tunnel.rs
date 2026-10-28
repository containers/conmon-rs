//! Port forwarding over a WebSocket: the protocol, and SPDY tunneled
//! through binary messages.

use super::{
    BASE64_CHANNEL_PROTOCOL, CHANNEL_PROTOCOL, PortForward, SPDY_TUNNEL_PROTOCOL, TUNNEL_BUFFER,
};
use crate::streaming_server::{
    stream_protocol::first_supported,
    web::{self, Message, WebSocket},
};
use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use std::pin::pin;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream},
    select, time,
};
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// A protocol a client may forward ports with over a WebSocket.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WebSocketProtocol {
    /// SPDY tunneled through binary messages.
    SpdyTunnel,

    /// The channel protocol, its payloads base64 encoded in text messages or
    /// as they are in binary ones.
    Channels { base64: bool },
}

/// The subprotocols of the channel protocol.
const CHANNEL_PROTOCOLS: [(&str, WebSocketProtocol); 2] = [
    (
        CHANNEL_PROTOCOL,
        WebSocketProtocol::Channels { base64: false },
    ),
    (
        BASE64_CHANNEL_PROTOCOL,
        WebSocketProtocol::Channels { base64: true },
    ),
];

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
        first_supported(requested, &CHANNEL_PROTOCOLS).map(|(protocol, p)| (protocol, Some(p)))
    }
}

impl PortForward {
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
                self.serve_channels(socket, ports, base64).await;
            }
        }
        debug!("Port forward WebSocket ended");
    }

    /// Carry the bytes of the SPDY connection in binary messages, until
    /// either side ends.
    pub(super) async fn tunnel<T>(
        socket: WebSocketStream<web::ClientIo<T>>,
        connection: DuplexStream,
    ) where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let discard = socket.get_ref().discarder();
        let (mut sink, mut messages) = socket.split();
        let (mut from_server, mut to_server) = tokio::io::split(connection);
        // Cancelled once the client sent a message larger than the server
        // takes, the close tells it why.
        let too_large = CancellationToken::new();

        // The sink, once the close went out.
        let mut to_client = tokio::spawn({
            let too_large = too_large.clone();
            async move {
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
                                return None;
                            }
                        }
                    }
                }
                let close = web::close(too_large.is_cancelled(), "");
                sink.send(close).await.is_ok().then_some(sink)
            }
        });

        let from_client = async {
            while let Some(message) = messages.next().await {
                match message {
                    Ok(Message::Binary(data)) => {
                        // The SPDY connection ended, its close goes to the
                        // client, which still gets read until it answered,
                        // for a bounded time.
                        if to_server.write_all(&data).await.is_err() {
                            web::await_close_answer(&mut messages).await;
                            break;
                        }
                    }
                    Ok(Message::Text(_)) => {
                        debug!("Ending port forward tunnel, the client sent a text message");
                        break;
                    }
                    Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                    Err(e) if web::is_too_large(&e) => {
                        debug!("Ending port forward tunnel: {e}");
                        too_large.cancel();
                        // It may still be busy writing the rest, which holds
                        // up the close for it.
                        discard.discard();
                        break;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                }
            }
            // Ends the SPDY connection, which ends the output as well.
            let _ = to_server.shutdown().await;
        };

        let sink = {
            let mut from_client = pin!(from_client);
            let closed_first = select! {
                () = &mut from_client => None,
                sink = &mut to_client => Some(sink.ok().flatten()),
            };
            match closed_first {
                None => web::join_or_abort(to_client).await.flatten(),
                // The SPDY connection ended, the client answers the close.
                Some(Some(sink)) => {
                    let _ = time::timeout(web::CLOSE_TIMEOUT, from_client).await;
                    Some(sink)
                }
                Some(None) => None,
            }
        };
        // A client which sent a message which was too large cannot answer.
        if let Some(sink) = sink.filter(|_| too_large.is_cancelled()) {
            web::drain(sink, messages).await;
        }
    }
}
