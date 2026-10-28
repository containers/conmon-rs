use super::{Decoder, Encoder, MAX_HEADER_BLOCK_LEN};
use futures::FutureExt;

/// Decode frames until the input ends or is invalid. Reading a slice is
/// never pending, so no runtime is needed.
pub fn frames(mut input: &[u8]) {
    let mut decoder = Decoder::default();
    while let Some(Ok(Some(_))) = decoder.read(&mut input).now_or_never() {}
}

/// Decode the input as a compressed header block, which is what a
/// client controls, parse it as a decompressed one, and decode it once
/// deflated like the server does, which always gets past the
/// decompression to the parsing. Headers which got parsed come back the
/// same once encoded, unless they exceed the limit.
pub fn header_block(raw: &[u8]) {
    let _ = Decoder::default().header_block(raw);
    let block = Encoder::default().deflate(raw);
    let decoded = Decoder::default().header_block(&block);
    let Ok(headers) = Decoder::parse_header_block(raw) else {
        assert!(decoded.is_err());
        return;
    };
    let serialized = Encoder::serialize(&headers);
    let decoded = Decoder::default().header_block(&Encoder::default().deflate(&serialized));
    if serialized.len() <= MAX_HEADER_BLOCK_LEN {
        assert_eq!(decoded.ok(), Some(headers));
    } else {
        assert!(decoded.is_err());
    }
}
