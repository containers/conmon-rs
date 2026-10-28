//! The output of a container split into the lines of the log, compared
//! with the output, see `conmonrs::fuzzing::log_lines`.
#![no_main]

use conmonrs::fuzzing::log_lines;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| log_lines(data));
