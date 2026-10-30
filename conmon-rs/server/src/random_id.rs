//! Random IDs, for the tokens of the streaming server, which a client has to
//! know to connect, and for names which must not collide.

use std::{fmt, str::FromStr};

/// 128 random bits from the operating system, written as 32 lowercase hex
/// digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RandomId(u128);

impl RandomId {
    /// The length of the written ID.
    const LEN: usize = 32;

    /// A new random ID. Panics if the operating system has no randomness,
    /// which Linux always has.
    pub fn new() -> Self {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).expect("random bytes of the operating system");
        Self(u128::from_ne_bytes(bytes))
    }
}

impl fmt::Display for RandomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// The first 8 hex digits of [`fmt::Display`], which tell the IDs in a log
/// apart without handing them out: the token of a request which got rejected
/// stays valid.
impl fmt::Debug for RandomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}", self.0 >> 96)
    }
}

/// Only exactly 32 lowercase hex digits, the way they get written, without
/// a sign or other characters.
impl FromStr for RandomId {
    type Err = InvalidRandomId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != Self::LEN || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(InvalidRandomId);
        }
        u128::from_str_radix(s, 16)
            .map(Self)
            .map_err(|_| InvalidRandomId)
    }
}

/// A string which is no [`RandomId`].
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidRandomId;

impl fmt::Display for InvalidRandomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expected {} lowercase hex digits", RandomId::LEN)
    }
}

impl std::error::Error for InvalidRandomId {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let id = RandomId::new();
        let written = id.to_string();
        assert_eq!(written.len(), RandomId::LEN);
        assert!(
            written
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_eq!(written.parse(), Ok(id));
        assert_ne!(RandomId::new(), id);

        // Leading zeros are written as well.
        assert_eq!(RandomId(1).to_string(), "00000000000000000000000000000001");
        assert_eq!(format!("{id:?}"), written[..8]);
        assert_eq!(format!("{:?}", RandomId(u128::MAX >> 4)), "0fffffff");
    }

    #[test]
    fn only_hex_digits_parse() {
        let valid = "0123456789abcdef0123456789abcdef";
        assert_eq!(valid.parse::<RandomId>().map(|_| ()), Ok(()));
        for invalid in [
            "",
            &valid[1..],
            &format!("{valid}0"),
            "+123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcdeF",
            "01234567-89ab-cdef-0123-456789abcdef",
        ] {
            assert_eq!(
                invalid.parse::<RandomId>(),
                Err(InvalidRandomId),
                "{invalid}"
            );
        }
    }
}
