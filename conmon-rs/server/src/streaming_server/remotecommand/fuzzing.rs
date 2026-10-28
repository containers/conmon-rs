use super::{
    CLOSE_BYTE, ClientEvent, Framing, Message, RESIZE_BYTE, STDIN_BYTE, Stdin, spdy::ResizeEvents,
    websocket::WEBSOCKET_PROTOCOLS,
};
use bytes::Bytes;
use serde::de::IgnoredAny;

/// The resize stream of a SPDY client, split into parts at NUL bytes.
/// Every event is a single JSON value, without whitespace around it.
pub fn resize_events(data: &[u8]) {
    let mut events = ResizeEvents::default();
    for part in data.split(|b| *b == 0) {
        events.push(part);
        while let Some(event) = events.pop() {
            assert!(!event.is_empty() && event.trim_ascii().len() == event.len());
            assert!(serde_json::from_slice::<IgnoredAny>(&event).is_ok());
        }
    }
}

/// The messages of a WebSocket client, for the framing the first byte
/// picks. Each message is a byte for its type, a byte for its length and
/// its payload. The events of each go to `event`, binary ones get checked
/// against the framing.
pub fn messages(input: &[u8], mut event: impl FnMut(ClientEvent)) {
    let Some((&framing, mut input)) = input.split_first() else {
        return;
    };
    let framing = WEBSOCKET_PROTOCOLS[usize::from(framing) % WEBSOCKET_PROTOCOLS.len()].1;
    while let [typ, len, rest @ ..] = input {
        let (payload, next) = rest.split_at(usize::from(*len).min(rest.len()));
        input = next;
        let data = Bytes::copy_from_slice(payload);
        let message = match typ % 5 {
            0 => Message::Binary(data.clone()),
            1 => Message::Text(String::from_utf8_lossy(payload).into_owned().into()),
            2 => Message::Close(None),
            3 => Message::Ping(data.clone()),
            _ => Message::Pong(data.clone()),
        };
        let binary = matches!(message, Message::Binary(_));
        let Some(got) = framing.event(message) else {
            continue;
        };
        if binary && framing == Framing::V5 {
            let expected = match payload {
                [STDIN_BYTE, stdin @ ..] => Some(ClientEvent::Stdin(Stdin::Data(
                    Bytes::copy_from_slice(stdin),
                ))),
                [CLOSE_BYTE, STDIN_BYTE] => Some(ClientEvent::Stdin(Stdin::Close)),
                [RESIZE_BYTE, resize @ ..] => {
                    Some(ClientEvent::Resize(Bytes::copy_from_slice(resize)))
                }
                _ => None,
            };
            assert_eq!(Some(&got), expected.as_ref());
        }
        if let ClientEvent::Stdin(Stdin::Data(stdin)) = &got {
            assert!(!stdin.is_empty());
        }
        event(got);
    }
}
