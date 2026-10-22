//! What the streaming protocols of Kubernetes share between exec, attach and
//! port forwarding, whatever the transport.

use std::{fmt, time::Duration};

/// The header which tells what a stream of a Kubernetes client is for.
pub const STREAM_TYPE_HEADER: &str = "streamType";

/// How long a Kubernetes client may take to open the streams which belong
/// together, the default of the kubelet and CRI-O.
pub const STREAM_CREATION_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of what a client sent a log shows.
const MAX_LOGGED_LEN: usize = 64;

/// A string of a client for the logs: quoted and escaped, so that it cannot
/// add lines to the log, and cut off after [`MAX_LOGGED_LEN`] characters.
pub struct ClientStr<'a>(pub &'a str);

impl fmt::Debug for ClientStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.char_indices().nth(MAX_LOGGED_LEN) {
            Some((end, _)) => write!(f, "{:?}...", &self.0[..end]),
            None => write!(f, "{:?}", self.0),
        }
    }
}

/// Bytes of a client for the logs, like [`ClientStr`].
pub struct ClientBytes<'a>(pub &'a [u8]);

impl fmt::Debug for ClientBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let end = self.0.len().min(MAX_LOGGED_LEN);
        write!(f, "\"{}\"", self.0[..end].escape_ascii())?;
        if end < self.0.len() {
            write!(f, "...")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_strings_get_escaped_and_cut_off() {
        assert_eq!(format!("{:?}", ClientStr("a\nb")), r#""a\nb""#);
        let long = "x".repeat(MAX_LOGGED_LEN + 1);
        assert_eq!(
            format!("{:?}", ClientStr(&long)),
            format!("{:?}...", &long[..MAX_LOGGED_LEN])
        );
        assert_eq!(
            format!("{:?}", ClientStr(&long[1..])),
            format!("{:?}", &long[1..])
        );
        // Not within a character.
        let wide = "\u{e9}".repeat(MAX_LOGGED_LEN + 1);
        assert!(format!("{:?}", ClientStr(&wide)).ends_with("\"..."));

        assert_eq!(format!("{:?}", ClientBytes(b"a\n\xff")), r#""a\n\xff""#);
        assert_eq!(
            format!("{:?}", ClientBytes(long.as_bytes())),
            format!("\"{}\"...", &long[..MAX_LOGGED_LEN])
        );
    }
}
