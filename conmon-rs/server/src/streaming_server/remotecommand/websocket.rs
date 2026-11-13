//! Exec and attach over a WebSocket: the framing of each subprotocol and
//! the input of the client.

use super::{
    CLOSE_BYTE, ClientEvent, ClientInput, Output, RESIZE_BYTE, STDIN_BYTE, Stdin, Transport,
    Version,
};
use crate::streaming_server::{
    channel,
    idle::Activity,
    stream_protocol::{ClientBytes, ClientStr, first_supported},
    web::{self, ClientMessage, Message, WebSocket},
};
use bytes::Bytes;
use futures::stream::SplitStream;
use tracing::{debug, trace};

/// How the messages of a WebSocket session look.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Framing {
    pub(crate) version: Version,

    /// Whether the payloads are base64 encoded in text messages, see
    /// [`channel`].
    pub(crate) base64: bool,
}

/// The WebSocket subprotocols, without one a client gets binary `v1`.
pub(super) const WEBSOCKET_PROTOCOLS: [(&str, Framing); 5] = [
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
    /// The framing of `v5.channel.k8s.io`.
    pub(crate) const V5: Self = Self {
        version: Version::V5,
        base64: false,
    };

    /// The framing for the subprotocols the client requested, in the order
    /// of the client, with the subprotocol the server answers with.
    pub(crate) fn negotiate<'a>(
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
        first_supported(requested, &WEBSOCKET_PROTOCOLS).map(|(framing, p)| (framing, Some(p)))
    }

    /// A message of the server on a channel.
    pub(crate) fn message(self, channel: u8, data: &[u8]) -> Message {
        channel::message(self.base64, channel, data)
    }

    /// What a message of the client means, none for one which needs no
    /// handling. Like for Kubernetes, text messages carry the same as binary
    /// ones, also for v5, its server does not look at the type.
    pub(crate) fn event(self, message: Message) -> Option<ClientEvent> {
        let data = match message {
            Message::Binary(data) => data,
            Message::Text(text) => Bytes::from(text),
            Message::Close(c) => {
                if let Some(cf) = c {
                    debug!(
                        "Got WebSocket close with code {} and reason {:?}",
                        cf.code,
                        ClientStr(&cf.reason)
                    );
                } else {
                    debug!("Got WebSocket close without close frame");
                }
                return Some(ClientEvent::Closed);
            }
            // Skipped by web::next_message, reads never return raw frames.
            Message::Pong(_) | Message::Ping(_) | Message::Frame(_) => return None,
        };

        if self.version == Version::V5 {
            let Some(&channel) = data.first() else {
                trace!("Got empty message");
                return None;
            };
            trace!("Got {} bytes", data.len());
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

/// The input of a WebSocket client.
pub(crate) struct WebSocketInput {
    pub(crate) messages: SplitStream<WebSocket>,
    pub(crate) framing: Framing,

    /// Touched by every message of the client except for pings and pongs,
    /// which a client like the WebSocket executor of client-go sends as a
    /// heartbeat. CRI-O does not count them either.
    pub(crate) activity: Activity,

    /// Touched whenever a part of what the server sends went out, see
    /// [`web::ClientIo::written`].
    pub(crate) written: Activity,

    /// Drops the input of a client which sent a message larger than the
    /// server takes, see [`web::Discard`].
    pub(crate) discard: web::Discard,
}

impl ClientInput for WebSocketInput {
    async fn next(&mut self) -> Option<ClientEvent> {
        loop {
            let message = match web::next_message(&mut self.messages, &self.discard).await {
                ClientMessage::Data(message) => message,
                // The framing logs it and ends the input.
                ClientMessage::Closed(frame) => Message::Close(frame),
                ClientMessage::TooLarge => return Some(ClientEvent::TooLarge),
                ClientMessage::Gone => return None,
            };
            self.activity.touch();
            if let Some(event) = self.framing.event(message) {
                return Some(event);
            }
        }
    }

    async fn answer_close(&mut self) {
        web::answer_close(&mut self.messages, &self.written).await;
    }

    async fn drain(self, output: Output) {
        if let Transport::WebSocket { sender, .. } = output.transport {
            web::drain(sender, self.messages).await;
        }
    }
}
