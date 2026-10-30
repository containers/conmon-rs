use super::Stat;

/// Parse the stat file of a process, whose command name the process sets
/// itself, so the one of a container. Whatever the name holds, also a
/// parenthesis and a space, the fields after it are found.
pub fn stat(name: &[u8]) {
    let _ = Stat::parse(name);

    let mut stat = b"1 (".to_vec();
    stat.extend_from_slice(name);
    stat.extend_from_slice(b") S 2 3 4 5\n");
    assert_eq!(
        Stat::parse(&stat),
        Some(Stat {
            state: 'S',
            ppid: 2,
            pgrp: 3,
        })
    );
}
