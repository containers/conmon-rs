//! The stat files of processes, whose command names containers set, see
//! `conmonrs::fuzzing::proc_stat`.
#![no_main]

use conmonrs::fuzzing::proc_stat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| proc_stat(data));
