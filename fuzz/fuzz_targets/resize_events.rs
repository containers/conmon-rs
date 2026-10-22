//! The resize stream of a SPDY client, split into parts at NUL bytes.
#![no_main]

use conmonrs::fuzzing::ResizeEvents;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut events = ResizeEvents::default();
    for part in data.split(|b| *b == 0) {
        events.push(part);
        while events.pop().is_some() {}
    }
});
