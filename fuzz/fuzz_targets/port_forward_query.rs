//! The query of a port forward request over the WebSocket channel protocol.
#![no_main]

use conmonrs::fuzzing::query_ports;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| query_ports(&String::from_utf8_lossy(data)));
