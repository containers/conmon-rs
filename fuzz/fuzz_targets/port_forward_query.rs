//! The query of a port forward request over the WebSocket channel protocol.
#![no_main]

use conmonrs::fuzzing::port_forward_query;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    port_forward_query(&String::from_utf8_lossy(data));
});
