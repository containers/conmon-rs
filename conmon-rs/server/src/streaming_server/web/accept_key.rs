//! The `Sec-WebSocket-Accept` header of the WebSocket handshake.

use data_encoding::BASE64;

/// The GUID which RFC 6455 appends to the key of the client.
const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The value of the `Sec-WebSocket-Accept` header for the
/// `Sec-WebSocket-Key` of a client, see RFC 6455, section 4.2.2: the base64
/// of the SHA-1 of the key and a fixed GUID.
///
/// The hash is an echo of the protocol, which tells the client that the
/// server speaks WebSocket. It is no security boundary, so SHA-1 being
/// broken for signatures does not matter here, and it is not worth the
/// crates of a hash library.
pub(super) fn accept_key(key: &[u8]) -> String {
    BASE64.encode(&sha1(&[key, GUID].concat()))
}

/// The SHA-1 digest of `data`, see RFC 3174.
fn sha1(data: &[u8]) -> [u8; 20] {
    // The message gets padded to whole blocks of 64 bytes: a 1 bit, zeros,
    // and the length of the data in bits as the last 8 bytes.
    let mut message = data.to_vec();
    message.push(0x80);
    message.resize((message.len() + 8).next_multiple_of(64) - 8, 0);
    let bits = (data.len() as u64).wrapping_mul(8);
    message.extend_from_slice(&bits.to_be_bytes());

    let mut state: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    for block in message.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for t in 16..80 {
            w[t] = (w[t - 3] ^ w[t - 8] ^ w[t - 14] ^ w[t - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (t, word) in w.iter().enumerate() {
            let (f, k) = match t {
                0..20 => ((b & c) | (!b & d), 0x5A82_7999),
                20..40 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..60 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            (a, b, c, d, e) = (temp, a, b.rotate_left(30), c, d);
        }
        for (sum, value) in state.iter_mut().zip([a, b, c, d, e]) {
            *sum = sum.wrapping_add(value);
        }
    }

    let mut digest = [0; 20];
    for (bytes, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *bytes = word.to_be_bytes();
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_encoding::HEXLOWER;

    #[test]
    fn accept_key_of_rfc_6455() {
        assert_eq!(
            accept_key(b"dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn sha1_test_vectors() {
        for (data, repeat, digest) in [
            // The test vectors of RFC 3174.
            ("abc", 1, "a9993e364706816aba3e25717850c26c9cd0d89d"),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                1,
                "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
            ),
            ("a", 1_000_000, "34aa973cd4c4daa4f61eeb2bdbad27316534016f"),
            (
                "0123456701234567012345670123456701234567012345670123456701234567",
                10,
                "dea356a2cddd90c7a7ecedc5ebb563934f460452",
            ),
            // Around the sizes where the padding takes another block.
            ("", 1, "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
            ("a", 55, "c1c8bbdc22796e28c0e15163d20899b65621d65a"),
            ("a", 56, "c2db330f6083854c99d4b5bfb6e8f29f201be699"),
            ("a", 63, "03f09f5b158a7a8cdad920bddc29b81c18a551f5"),
            ("a", 64, "0098ba824b5c16427bd7a1122a5a442a25ec644d"),
            ("a", 65, "11655326c708d70319be2610e8a57d9a5b959d3b"),
        ] {
            assert_eq!(
                HEXLOWER.encode(&sha1(data.repeat(repeat).as_bytes())),
                digest,
                "{repeat} times {data:?}"
            );
        }
    }
}
