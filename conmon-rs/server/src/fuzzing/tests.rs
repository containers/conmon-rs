use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The entry point of a fuzz target, which returns whether it accepted
/// the input where it can tell.
type Target = fn(&[u8]) -> Option<bool>;

/// The entry points of the fuzz targets by their names, like the targets
/// in `fuzz/fuzz_targets` call them.
const TARGETS: [(&str, Target); 15] = [
    ("cgroup_files", |data| {
        cgroup_files(data);
        None
    }),
    ("channel_decode", |data| {
        let (&base64, message) = data.split_first()?;
        Some(channel_decode(base64 & 1 == 1, message))
    }),
    ("client_messages", |data| Some(client_messages(data))),
    ("journal_entry", |data| Some(journal_entry(data))),
    ("log_lines", |data| {
        log_lines(data);
        None
    }),
    ("pending_stdin", |data| {
        pending_stdin(data);
        None
    }),
    ("port_forward_query", |data| {
        Some(port_forward_query(&String::from_utf8_lossy(data)))
    }),
    ("port_forward_spdy", |data| {
        port_forward_spdy(data);
        None
    }),
    ("proc_stat", |data| {
        proc_stat(data);
        None
    }),
    ("remotecommand_spdy", |data| {
        remotecommand_spdy(data);
        None
    }),
    ("resize_events", |data| Some(resize_events(data))),
    ("spdy_connection", |data| {
        spdy_connection(data);
        None
    }),
    ("spdy_frames", |data| {
        spdy_frames(data);
        None
    }),
    ("spdy_header_block", |data| Some(spdy_header_block(data))),
    ("upgrade_headers", |data| {
        upgrade_headers(data);
        None
    }),
];

/// The fuzz directory of the repository.
fn fuzz_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz")
}

/// Every target in `fuzz/fuzz_targets` has its entry point here and a binary
/// in the manifest.
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

    // cargo fuzz builds only the targets with a binary in the manifest,
    // and none else.
    let manifest = fs::read_to_string(fuzz_dir().join("Cargo.toml")).expect("fuzz manifest");
    for (name, _) in TARGETS {
        let bin = format!("[[bin]]\nname = \"{name}\"\npath = \"fuzz_targets/{name}.rs\"\n");
        assert!(
            manifest.contains(&bin),
            "{name} has no binary in the manifest"
        );
    }
    assert_eq!(manifest.matches("[[bin]]").count(), TARGETS.len());
}

/// Every target has seeds and takes them and short inputs, which the fuzzer
/// tries first, so that a target which fails on them fails the tests
/// already. The targets which can tell accept their seeds, so that seeds of
/// a format which went stale fail as well.
/// Unlike under libFuzzer, whose panic hook aborts on any panic, tokio
/// catches a panic in a task here. The targets wait for the tasks they
/// spawn themselves, so a panic in one of them still fails the test, one in
/// a task of the server only if the session then does not end.
#[test]
fn targets_take_their_seeds_and_short_inputs() {
    let seeds = fuzz_dir().join("seeds");
    for (name, target) in TARGETS {
        for input in [&[][..], &[0], &[0xff], &[0, 5], &[0xff; 2]] {
            eprintln!("{name}: {input:?}");
            target(input);
        }
        let dir = seeds.join(name);
        let mut count = 0;
        for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = entry.expect("seed").path();
            let input = fs::read(&path).expect("read seed");
            eprintln!("{name}: {input:?}");
            assert_ne!(
                target(&input),
                Some(false),
                "{} got rejected",
                path.display()
            );
            count += 1;
        }
        assert!(count > 0, "{name} has no seeds");
    }
}
