//! The streams of a SPDY port forward session, how they pair up into
//! requests, see `conmonrs::fuzzing::port_forward_spdy`.
#![no_main]

use conmonrs::fuzzing::port_forward_spdy;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| port_forward_spdy(data));
