//! Registering children and looking them up.

use super::*;

#[test]
fn forget_grandchild_drops_empty_entries() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let exit_path = dir.path().join("exit");
    let reapable = |pid| -> Result<ReapableChild> {
        Ok(ReapableChild::from_child(child(pid, None, &exit_path, &[])?).1)
    };
    // The PID of the first one got reused.
    let (first, second, third) = (
        reapable(Pid::from_raw(1))?,
        reapable(Pid::from_raw(1))?,
        reapable(Pid::from_raw(3))?,
    );
    let (first_seq, third_seq) = (first.seq, third.seq);
    let second_seq = second.seq;
    let map = Arc::new(Mutex::new(HashMap::from([
        ("a".into(), vec![first, second]),
        ("b".into(), vec![third]),
    ])));

    ChildReaper::forget_grandchild(&map, "b", third_seq);
    ChildReaper::forget_grandchild(&map, "a", first_seq);
    // Unknown registrations are ignored.
    ChildReaper::forget_grandchild(&map, "a", first_seq);
    ChildReaper::forget_grandchild(&map, "c", first_seq);

    let map = lock(&map);
    assert_eq!(map.len(), 1);
    assert_eq!(
        map["a"].iter().map(|c| c.seq).collect::<Vec<_>>(),
        [second_seq]
    );
    Ok(())
}

/// Clients match the start of the error, see `ErrContainerNotFound` of
/// the Go client.
#[test]
fn get_unknown_container_fails_with_stable_error() {
    let err = ChildReaper::default()
        .get("unknown")
        .err()
        .map(|e| format!("{e:#}"));
    assert_eq!(err.as_deref(), Some("container not found: unknown"));
}

#[tokio::test]
async fn registration_keeps_the_child_claimed() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let sh = spawn_sh("exec sleep 60")?;
    // As `create_child` leaves it.
    let mut call = reaper.begin_call(distant_deadline())?;
    call.claim(&reaper.pending, sh.pid);
    assert!(reaper.is_claimed(sh.pid));

    let child = child(sh.pid, None, &dir.path().join("exit"), &[])?;
    reaper.watch_grandchild(child, vec![], call)?;
    assert!(reaper.is_claimed(sh.pid));
    assert!(!lock(&reaper.pending).contains_key(&sh.pid));
    assert!(!reaper.calls_in_progress());
    Ok(())
}

#[tokio::test]
async fn leaked_fds_stay_open_until_the_exit() -> Result<()> {
    use nix::{fcntl::OFlag, unistd::pipe2};
    use std::io::{ErrorKind, Read};

    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let sh = spawn_sh("exec sleep 60")?;
    let (reader, writer) = pipe2(OFlag::O_NONBLOCK)?;
    let mut reader = std::fs::File::from(reader);
    let call = reaper.begin_call(distant_deadline())?;
    let child = child(sh.pid, None, &dir.path().join("exit"), &[])?;
    let mut exit_rx = reaper.watch_grandchild(child, vec![writer], call)?;

    // The server keeps the descriptor while the process runs.
    let mut buf = [0; 1];
    assert_eq!(
        reader.read(&mut buf).map_err(|e| e.kind()),
        Err(ErrorKind::WouldBlock)
    );

    sh.process.signal(Signal::SIGKILL)?;
    exit_rx.recv().await?;
    wait_for_real(|| matches!((&reader).read(&mut [0; 1]), Ok(0))).await;
    assert!(matches!(reader.read(&mut buf), Ok(0)));
    Ok(())
}
