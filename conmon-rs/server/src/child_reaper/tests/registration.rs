//! Registering children and looking them up.

use super::*;

#[test]
fn forget_grandchild_drops_empty_entries() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let exit_path = dir.path().join("exit");
    let reapable = |pid| -> Result<ReapableChild> {
        Ok(ReapableChild::from_child(child(pid, None, &exit_path)?).1)
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

    let child = child(sh.pid, None, &dir.path().join("exit"))?;
    reaper.watch_grandchild(child, call)?;
    assert!(reaper.is_claimed(sh.pid));
    assert!(!lock(&reaper.pending).contains_key(&sh.pid));
    assert!(!reaper.calls_in_progress());
    Ok(())
}

/// A child stays registered after its watcher reaped it, while the watcher
/// finishes the output. Another child may get its PID meanwhile, an orphan
/// with it is not claimed.
#[tokio::test]
async fn reaped_children_do_not_claim_their_pid() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let sh = spawn_sh("exec sleep 60")?;
    let pid = sh.pid;
    let child = child(pid, None, &dir.path().join("exit"))?;
    let (id, reapable, exit) = ReapableChild::from_child(child);
    lock(&reaper.grandchildren)
        .entry(id)
        .or_default()
        .push(reapable.clone());
    // A watcher which does not forget the child, like one which still
    // waits for the output.
    let mut exit_rx = reapable.watch(exit, || {});
    assert!(reaper.is_claimed(pid));

    sh.process.signal(Signal::SIGKILL)?;
    time::timeout(Duration::from_secs(60), exit_rx.recv()).await??;
    assert!(!lock(&reaper.grandchildren).is_empty());
    assert!(!reaper.is_claimed(pid));
    Ok(())
}
