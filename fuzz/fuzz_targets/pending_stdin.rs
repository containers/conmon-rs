//! The stdin a session holds while the container does not take it, compared
//! with a model, see `conmonrs::fuzzing::pending_stdin`.
#![no_main]

use conmonrs::fuzzing::pending_stdin;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| pending_stdin(data));
