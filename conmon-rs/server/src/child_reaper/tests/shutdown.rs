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

    // New calls do not start at all. The runtime would write the PID file,
    // the call would succeed otherwise.
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let pidfile = dir.path().join("pid");
    let args = ["-c".into(), format!("echo $$ >{}", pidfile.display())];
    let res = reaper
        .create_child(
            RuntimeCall::new(Path::new("sh"), &args, &pidfile, distant_deadline()),
            &mut io,
        )
        .await;
    let err = res.err().map(|e| format!("{e:#}"));
    assert_eq!(err.as_deref(), Some("server is shutting down"));
    assert!(!pidfile.exists());
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
    let mut call = reaper.begin_call(distant_deadline())?;
    let started = spawn_sh("exec sleep 60")?;
    call.claim(&reaper.pending, started.pid);
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
    // The call ended in time, which registers its process or kills it.
    assert!(started.process.is_running());

    // A call which does not end in time gets stopped, and the shutdown
    // reports it. Like one whose runtime exited, which waits for the output
    // of the runtime, the process it claimed but did not register yet,
    // like a created container, does not stay behind without a watcher.
    // One which never ends does not keep the shutdown beyond its deadline.
    // Close to the deadline, a call gets stopped early enough to end before
    // it, like after the grace of the containers on SIGTERM.
    for (deadline, ends, waited, expected) in [
        (
            Duration::from_secs(60),
            true,
            ChildReaper::IN_FLIGHT_WAIT,
            "runtime calls in progress got stopped",
        ),
        (
            Duration::from_secs(3),
            true,
            Duration::from_secs(3) - ChildReaper::STOP_BUDGET,
            "runtime calls in progress got stopped",
        ),
        (
            Duration::from_secs(60),
            false,
            Duration::from_secs(60),
            "runtime calls still in progress",
        ),
        (
            Duration::from_secs(2),
            false,
            Duration::from_secs(2),
            "runtime calls still in progress",
        ),
    ] {
        let reaper = ChildReaper::default();
        let mut call = reaper.begin_call(distant_deadline())?;
        let started = spawn_sh("exec sleep 60")?;
        call.claim(&reaper.pending, started.pid);
        // It does not kill the process itself.
        let _call = if ends {
            task::spawn(async move {
                call.stop.cancelled().await;
                drop(call);
            });
            None
        } else {
            Some(call)
        };
        let start = Instant::now();
        let err = reaper
            .kill_grandchildren(start + deadline)
            .await
            .err()
            .map(|e| format!("{e:#}"));
        assert_eq!(err.as_deref(), Some(expected));
        assert_eq!(start.elapsed(), waited);
        assert!(matches!(
            waitpid(started.pid, None)?,
            WaitStatus::Signaled(_, Signal::SIGKILL, _)
        ));
    }

    // A call which claims its process after the shutdown stopped it kills
    // the process itself.
    let reaper = ChildReaper::default();
    let mut call = reaper.begin_call(distant_deadline())?;
    let deadline = Instant::now() + Duration::from_secs(1);
    assert!(reaper.kill_grandchildren(deadline).await.is_err());
    let late = spawn_sh("exec sleep 60")?;
    call.claim(&reaper.pending, late.pid);
    assert!(matches!(
        waitpid(late.pid, None)?,
        WaitStatus::Signaled(_, Signal::SIGKILL, _)
    ));
    drop(call);

    // A runtime which still runs gets killed by its call, which fails. The
    // shutdown does not signal it by its PID, which tokio reaps. Stopped
    // right away, the call ends within the budget of the stop, also with a
    // terminal, whose runtime has pipes of its own, which a helper in a
    // session of its own may hold open.
    time::resume();
    for terminal in [false, true] {
        let reaper = Arc::new(ChildReaper::default());
        let dir = tempfile::tempdir()?;
        let runtime_pidfile = dir.path().join("runtime");
        let call = task::spawn({
            let reaper = reaper.clone();
            let pidfile = dir.path().join("pid");
            let script = format!(
                "setsid sleep 5 & echo $$ >{}; exec sleep 60",
                runtime_pidfile.display()
            );
            let args = ["-c".into(), script];
            async move {
                let mut io = ContainerIO::new(terminal, ContainerLog::new())?;
                let call = RuntimeCall::new(Path::new("sh"), &args, &pidfile, distant_deadline());
                reaper.create_child(call, &mut io).await.map(drop)
            }
        });
        wait_for_real(|| read_pid_file(&runtime_pidfile).is_ok()).await;
        let runtime = read_pid_file(&runtime_pidfile)?;
        let deadline = Instant::now() + ChildReaper::STOP_BUDGET;
        let err = reaper.kill_grandchildren(deadline).await.err();
        assert_eq!(
            err.map(|e| format!("{e:#}")).as_deref(),
            Some("runtime calls in progress got stopped"),
            "terminal: {terminal}"
        );
        let err = call.await?.err().map(|e| format!("{e:#}"));
        assert_eq!(err.as_deref(), Some("runtime got stopped by the shutdown"));
        assert!(is_gone(runtime));
        assert!(lock(&reaper.pending).is_empty());
        assert!(!reaper.calls_in_progress());
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

#[tokio::test]
async fn exited_children_get_only_their_group_signalled() -> Result<()> {
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    // A container which its watcher reaped, whose token is not cancelled
    // yet, and a process left in its group which holds its pipes.
    let mut leader = Guarded::spawn(Command::new("sleep").arg("60").process_group(0))?;
    let pid = child_pid(leader.0.id())?;
    let mut member = Guarded::spawn(Command::new("sleep").arg("60").process_group(pid.as_raw()))?;
    let (id, exited, _) =
        ReapableChild::from_child(child(pid, None, &dir.path().join("exit"), &[])?);
    lock(&reaper.grandchildren).insert(id, vec![exited]);
    leader.0.kill()?;
    leader.0.wait()?;

    let start = Instant::now();
    let wait = Duration::from_millis(100);
    reaper.terminate_containers(start + wait).await;
    // Registered without a watcher, which would forget it.
    let err = reaper
        .kill_grandchildren(start + 2 * wait)
        .await
        .err()
        .map(|e| format!("{e:#}"));
    assert_eq!(err, Some(format!("PIDs [{pid}] did not exit")));
    assert_eq!(member.0.wait()?.signal(), Some(Signal::SIGKILL as i32));
    Ok(())
}
