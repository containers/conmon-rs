//! The cgroup files of the kernel which the OOM watcher reads, see
//! `conmonrs::fuzzing::cgroup_files`.
#![no_main]

use conmonrs::fuzzing::cgroup_files;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| cgroup_files(data));
