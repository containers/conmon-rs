//! The frames a SPDY client sends over a connection, see
//! `conmonrs::fuzzing::spdy_connection`.
#![no_main]

use conmonrs::fuzzing::spdy_connection;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| spdy_connection(data));
