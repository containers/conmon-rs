//! The filter of the notify messages of a container, see
//! `conmonrs::fuzzing::notify_filter`.
#![no_main]

use conmonrs::fuzzing::notify_filter;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| notify_filter(data));
