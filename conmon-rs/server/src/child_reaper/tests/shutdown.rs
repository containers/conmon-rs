//! Terminating and killing the children at the shutdown.

use super::*;

#[tokio::test]
async fn registration_after_shutdown_kills_child() -> Result<()> {
    let reaper = ChildReaper::default();
    reaper
        .kill_grandchildren(Instant::now() + Duration::from_secs(10))
        .await?;

    // A call which was in progress while the shutdown killed the others.
    let dir = tempfile::tempdir()?;
    let sh = spawn_sh("exec sleep 60")?;
    let pid = sh.pid;
    assert!(
        reaper
            .watch_grandchild(
                child(pid, None, &dir.path().join("exit"), &[])?,
                vec![],
                InFlight::default()
            )
            .is_err()
    );
    wait_for_real(|| is_gone(pid)).await;

    // New calls do not start at all.
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let pidfile = dir.path().join("pid");
    let args = ["x".into()];
    let res = reaper
        .create_child(
            RuntimeCall::new(Path::new("true"), &args, &pidfile, distant_deadline()),
            &mut io,
        )
        .await;
    assert!(res.is_err());
    Ok(())
}

#[test]
fn late_grandchildren_which_are_gone_are_not_signalled() -> Result<()> {
    let mut reaped = Command::new("true").spawn()?;
    let pid = child_pid(reaped.id())?;
    reaped.wait()?;
    assert!(!ChildReaper::kill_late_grandchild(pid));

    let sh = spawn_sh("exec sleep 60")?;
    assert!(ChildReaper::kill_late_grandchild(sh.pid));
    assert!(matches!(
        waitpid(sh.pid, None)?,
        WaitStatus::Signaled(_, Signal::SIGKILL, _)
    ));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_calls_in_progress() -> Result<()> {
    let reaper = Arc::new(ChildReaper::default());
    let call = reaper.begin_call(distant_deadline())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let shutdown = task::spawn({
        let reaper = reaper.clone();
        async move { reaper.kill_grandchildren(deadline).await }
    });
    time::sleep(Duration::from_secs(1)).await;
    assert!(!shutdown.is_finished());
    // No new call starts meanwhile.
    assert!(reaper.begin_call(distant_deadline()).is_err());

    drop(call);
    shutdown.await??;

    // A call which never ends does not keep the shutdown forever, and
    // the shutdown reports it.
    for (deadline, waited) in [
        (Duration::from_secs(60), ChildReaper::IN_FLIGHT_WAIT),
        (Duration::from_secs(2), Duration::from_secs(2)),
    ] {
        let reaper = ChildReaper::default();
        let _call = reaper.begin_call(distant_deadline())?;
        let start = Instant::now();
        let err = reaper
            .kill_grandchildren(start + deadline)
            .await
            .err()
            .map(|e| format!("{e:#}"));
        assert_eq!(err.as_deref(), Some("runtime calls still in progress"));
        assert_eq!(start.elapsed(), waited);
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_does_not_wait_forever_for_a_watcher() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let sh = spawn_sh("exec sleep 60")?;
    // Registered without a watcher, like a child whose watcher never ends
    // because the process is stuck in the kernel.
    let (id, stuck, _) =
        ReapableChild::from_child(child(sh.pid, None, &dir.path().join("exit"), &[])?);
    lock(&reaper.grandchildren).insert(id, vec![stuck]);

    let start = Instant::now();
    let err = reaper
        .kill_grandchildren(start + Duration::from_secs(8))
        .await
        .err()
        .map(|e| format!("{e:#}"));
    assert_eq!(err, Some(format!("PIDs [{}] did not exit", sh.pid)));
    assert_eq!(start.elapsed(), Duration::from_secs(8));
    Ok(())
}

#[tokio::test]
async fn terminate_signals_the_containers_only() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let container = spawn_sh("exec sleep 60")?;
    let registered = child(container.pid, None, &dir.path().join("container"), &[])?;
    reaper.watch_grandchild(registered, vec![], InFlight::default())?;
    let exec = spawn_sh("exec sleep 60")?;
    let registered = Child {
        kind: ChildKind::Exec,
        ..child(exec.pid, None, &dir.path().join("exec"), &[])?
    };
    reaper.watch_grandchild(registered, vec![], InFlight::default())?;

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut terminate = pin!(reaper.terminate_containers(deadline));
    // Signals on the first poll, the watchers did not run yet.
    assert!((&mut terminate).now_or_never().is_none());
    // A container registered later gets neither signalled nor waited for.
    let late = spawn_sh("exec sleep 60")?;
    let registered = Child {
        id: "late".into(),
        ..child(late.pid, None, &dir.path().join("late"), &[])?
    };
    reaper.watch_grandchild(registered, vec![], InFlight::default())?;
    time::timeout(Duration::from_secs(20), terminate).await?;

    let terminated = 128 + Signal::SIGTERM as i32;
    let code = std::fs::read_to_string(dir.path().join("container"))?;
    assert_eq!(code, terminated.to_string());
    assert!(exec.process.is_running());
    assert!(late.process.is_running());
    reaper.kill_grandchildren(deadline).await
}

#[tokio::test]
async fn terminate_waits_until_the_deadline() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    // Like the init of a PID namespace without a handler for SIGTERM, once
    // it ignores it.
    let ignoring = spawn_sh("trap '' TERM; exec sleep 60")?;
    let comm = format!("/proc/{}/comm", ignoring.pid);
    wait_for_real(|| std::fs::read_to_string(&comm).is_ok_and(|c| c == "sleep\n")).await;
    let registered = child(ignoring.pid, None, &dir.path().join("container"), &[])?;
    reaper.watch_grandchild(registered, vec![], InFlight::default())?;

    let start = Instant::now();
    let grace = Duration::from_millis(300);
    reaper.terminate_containers(start + grace).await;
    assert!(start.elapsed() >= grace);
    assert!(ignoring.process.is_running());
    reaper.kill_grandchildren(start + grace * 100).await
}

#[tokio::test]
async fn shutdown_waits_for_the_watchers() -> Result<()> {
    let reaper = Arc::new(ChildReaper::default());
    let dir = tempfile::tempdir()?;
    let running = spawn_sh("exec sleep 60")?;
    let call = reaper.begin_call(distant_deadline())?;
    let registered = child(running.pid, None, &dir.path().join("exit"), &[])?;
    reaper.watch_grandchild(registered, vec![], call)?;

    // A call in progress registers its child after the snapshot.
    let call = reaper.begin_call(distant_deadline())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let shutdown = task::spawn({
        let reaper = reaper.clone();
        async move { reaper.kill_grandchildren(deadline).await }
    });
    wait_for_real(|| is_gone(running.pid)).await;
    let late = spawn_sh("exec sleep 60")?;
    let registered = child(late.pid, None, &dir.path().join("late"), &[])?;
    assert!(reaper.watch_grandchild(registered, vec![], call).is_err());

    time::timeout(Duration::from_secs(20), shutdown).await???;
    assert!(is_gone(late.pid));
    assert!(lock(&reaper.grandchildren).is_empty());
    Ok(())
}
