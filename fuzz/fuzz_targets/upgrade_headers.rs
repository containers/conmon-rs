//! The headers of a WebSocket or SPDY upgrade request, one per line.
#![no_main]

use conmonrs::fuzzing::upgrade_headers;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| upgrade_headers(data));
