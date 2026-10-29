//! The streams of a SPDY exec or attach session, how the session starts
//! and ends, see `conmonrs::fuzzing::remotecommand_spdy`.
#![no_main]

use conmonrs::fuzzing::remotecommand_spdy;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| remotecommand_spdy(data));
