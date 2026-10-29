//! The channel framing of the Kubernetes WebSocket protocols, which exec,
//! attach and port forwarding share: every message starts with its channel,
//! base64 protocols send text messages with the channel as an ASCII digit
//! offset and the payload base64 encoded.

use crate::streaming_server::web::Message;
use bytes::{BufMut, Bytes, BytesMut};
use data_encoding::BASE64;
use tracing::debug;

/// A message of the server on a channel.
pub fn message(base64: bool, channel: u8, data: &[u8]) -> Message {
    if base64 {
        let mut text = String::with_capacity(1 + data.len().div_ceil(3) * 4);
        text.push(char::from(b'0' + channel));
        BASE64.encode_append(data, &mut text);
        Message::Text(text.into())
    } else {
        let mut frame = BytesMut::with_capacity(1 + data.len());
        frame.put_u8(channel);
        frame.put_slice(data);
        Message::Binary(frame.freeze())
    }
}

/// The channel and the payload of a message of the client, none for an
/// empty one or invalid base64. Binary payloads do not get copied.
pub fn decode(base64: bool, message: Bytes) -> Option<(u8, Bytes)> {
    let &channel = message.first()?;
    if !base64 {
        return Some((channel, message.slice(1..)));
    }
    match BASE64.decode(&message[1..]) {
        Ok(payload) => Some((channel.wrapping_sub(b'0'), payload.into())),
        Err(e) => {
            debug!("Ignoring message with invalid base64: {e}");
            None
        }
    }
}

/// The entry points of the fuzz targets.
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzzing {
    use super::*;

    /// Decode the input as a message of a client. A message of the server
    /// with its first byte as the channel and the rest as the payload decodes
    /// to them again, for channels which base64 messages can carry as a
    /// single ASCII digit offset.
    pub fn channel(base64: bool, input: &[u8]) {
        let _ = decode(base64, Bytes::copy_from_slice(input));
        let Some((&channel, payload)) = input.split_first() else {
            return;
        };
        if base64 && channel > 0x7f - b'0' {
            return;
        }
        let message = match message(base64, channel, payload) {
            Message::Binary(data) => data,
            Message::Text(text) => Bytes::from(text),
            message => panic!("unexpected message {message:?}"),
        };
        assert_eq!(
            decode(base64, message),
            Some((channel, Bytes::copy_from_slice(payload)))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_get_framed() {
        assert_eq!(
            message(false, 1, b"out"),
            Message::Binary(Bytes::from_static(b"\x01out"))
        );
        assert_eq!(message(true, 2, b"err"), Message::Text("2ZXJy".into()));
        assert_eq!(message(true, 1, b""), Message::Text("1".into()));
    }

    #[test]
    fn messages_get_decoded() {
        assert_eq!(
            decode(false, Bytes::from_static(b"\x00ls")),
            Some((0, Bytes::from_static(b"ls")))
        );
        assert_eq!(
            decode(true, Bytes::from_static(b"0bHM=")),
            Some((0, Bytes::from_static(b"ls")))
        );
        assert_eq!(decode(false, Bytes::new()), None);
        assert_eq!(decode(true, Bytes::from_static(b"0!")), None);
    }
}
