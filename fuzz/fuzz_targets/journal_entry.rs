//! The fields of a journal entry, encoded and decoded again, see
//! `conmonrs::fuzzing::journal_entry`.
#![no_main]

use conmonrs::fuzzing::journal_entry;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| journal_entry(data));
