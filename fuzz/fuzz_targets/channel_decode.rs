//! The WebSocket channel messages of a client, the first byte picks base64.
#![no_main]

use bytes::Bytes;
use conmonrs::fuzzing::channel_decode;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some((&base64, message)) = data.split_first() {
        channel_decode(base64 & 1 == 1, Bytes::copy_from_slice(message));
    }
});
