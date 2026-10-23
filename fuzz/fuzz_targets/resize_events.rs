//! The resize stream of a SPDY client, split into parts at NUL bytes.
#![no_main]

use conmonrs::fuzzing::resize_events;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| resize_events(data));
