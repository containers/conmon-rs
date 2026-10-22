//! The SPDY frames of a client.
#![no_main]

use conmonrs::fuzzing::spdy_frames;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| spdy_frames(data));
