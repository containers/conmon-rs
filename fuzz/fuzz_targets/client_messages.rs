//! The WebSocket messages of an exec or attach client, for the framing the
//! first byte picks, each with a byte for its type and one for its length.
#![no_main]

use conmonrs::fuzzing::client_messages;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| client_messages(data));
