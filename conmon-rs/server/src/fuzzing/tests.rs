use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The entry point of a fuzz target.
type Target = fn(&[u8]);

/// The entry points of the fuzz targets by their names, like the targets
/// in `fuzz/fuzz_targets` call them.
const TARGETS: [(&str, Target); 14] = [
    ("channel_decode", |data| {
        if let Some((&base64, message)) = data.split_first() {
            channel(base64 & 1 == 1, message);
        }
    }),
    ("client_messages", client_messages),
    ("journal_entry", journal_entry),
    ("log_lines", log_lines),
    ("notify_filter", notify_filter),
    ("pending_stdin", pending_stdin),
    ("port_forward_query", |data| {
        query_ports(&String::from_utf8_lossy(data));
    }),
    ("port_forward_spdy", port_forward_spdy),
    ("remotecommand_spdy", remotecommand_spdy),
    ("resize_events", resize_events),
    ("spdy_connection", spdy_connection),
    ("spdy_frames", spdy_frames),
    ("spdy_header_block", spdy_header_block),
    ("upgrade_headers", upgrade_headers),
];

/// The fuzz directory of the repository.
fn fuzz_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz")
}

/// Every target in `fuzz/fuzz_targets` has its entry point here.
#[test]
fn all_targets_get_tested() {
    let mut names: Vec<_> = fs::read_dir(fuzz_dir().join("fuzz_targets"))
        .expect("fuzz targets")
        .map(|entry| {
            let path = entry.expect("fuzz target").path();
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
                .expect("file name")
        })
        .collect();
    names.sort();
    assert_eq!(names, TARGETS.map(|(name, _)| name));
}

/// Every target takes its seeds and short inputs, which the fuzzer tries
/// first, so that a target which fails on them fails the tests already.
/// Unlike under libFuzzer, whose panic hook aborts on any panic, tokio
/// catches a panic in a task here. The targets wait for the tasks they
/// spawn themselves, so a panic in one of them still fails the test, one in
/// a task of the server only if the session then does not end.
#[test]
fn targets_take_their_seeds_and_short_inputs() {
    let seeds = fuzz_dir().join("seeds");
    for (name, target) in TARGETS {
        let mut inputs: Vec<Vec<u8>> = vec![vec![], vec![0], vec![0xff], vec![0, 5], vec![0xff; 2]];
        let dir = seeds.join(name);
        for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            inputs.push(fs::read(entry.expect("seed").path()).expect("read seed"));
        }
        for input in inputs {
            eprintln!("{name}: {input:?}");
            target(&input);
        }
    }
}
