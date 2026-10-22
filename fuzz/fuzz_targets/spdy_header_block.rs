//! The header blocks of the SPDY frames of a client.
#![no_main]

use conmonrs::fuzzing::spdy_header_block;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| spdy_header_block(data));
