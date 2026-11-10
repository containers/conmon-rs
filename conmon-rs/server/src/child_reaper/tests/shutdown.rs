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
    let err = reaper
        .watch_grandchild(
            child(pid, None, &dir.path().join("exit"))?,
            InFlight::default(),
        )
        .err()
        .map(|e| format!("{e:#}"));
    assert_eq!(err.as_deref(), Some("server is shutting down"));
    wait_for_real(|| is_gone(pid)).await;
    // Its create fails, so it gets no exit file.
    let left = reaper
        .wait_for_watchers(Instant::now() + Duration::from_secs(10), |_| true)
        .await;
    assert!(left.is_empty(), "{left:?}");
    assert!(!dir.path().join("exit").exists());

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
    let err = reaper.begin_call(distant_deadline()).err();
    assert_eq!(
        err.map(|e| format!("{e:#}")).as_deref(),
        Some("server is shutting down")
    );

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
    let err = reaper.kill_grandchildren(deadline).await.err();
    assert_eq!(
        err.map(|e| format!("{e:#}")).as_deref(),
        Some("runtime calls still in progress")
    );
    let late = spawn_sh("exec sleep 60")?;
    call.claim(&reaper.pending, late.pid);
    assert!(matches!(
        waitpid(late.pid, None)?,
        WaitStatus::Signaled(_, Signal::SIGKILL, _)
    ));
    drop(call);

    // A runtime which still runs gets killed by its call, which fails. The
    // shutdown does not signal it by its PID, which tokio reaps. Stopped
    // right away, the call waits only briefly for the exit and the output
    // of its runtime, also with a terminal, whose runtime has pipes of its
    // own, which a helper in a session of its own holds open. Whether that
    // fits into the budget of the stop depends on the machine, a loaded
    // one or one under emulation may take longer. The cases above cover
    // the result of the shutdown, here the call has to end well before the
    // waits without a stop, the grace of the runtime, would let it.
    time::resume();
    for terminal in [false, true] {
        let reaper = Arc::new(ChildReaper::default());
        let dir = tempfile::tempdir()?;
        let runtime_pidfile = dir.path().join("runtime");
        let call = task::spawn({
            let reaper = reaper.clone();
            let pidfile = dir.path().join("pid");
            let script = format!(
                "setsid sleep 10 & echo $$ >{}; exec sleep 60",
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
        let start = Instant::now();
        let err = reaper
            .kill_grandchildren(start + ChildReaper::STOP_BUDGET)
            .await
            .err()
            .map(|e| format!("{e:#}"));
        assert!(
            matches!(
                err.as_deref(),
                Some("runtime calls in progress got stopped" | "runtime calls still in progress")
            ),
            "terminal: {terminal}, {err:?}"
        );
        let err = call.await?.err().map(|e| format!("{e:#}"));
        assert_eq!(err.as_deref(), Some("runtime got stopped by the shutdown"));
        let elapsed = start.elapsed();
        assert!(
            elapsed < ChildReaper::RUNTIME_GRACE,
            "terminal: {terminal}, {elapsed:?}"
        );
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
    let (id, stuck, _) = ReapableChild::from_child(child(sh.pid, None, &dir.path().join("exit"))?);
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
async fn shutdown_waits_for_timed_out_execs() -> Result<()> {
    let reaper = ChildReaper::default();
    let exec = spawn_sh("exec sleep 60")?;
    let registered = Child {
        kind: ChildKind::Exec,
        pid: exec.pid,
        timeout: Some(Instant::now() + Duration::from_millis(100)),
        ..container_child(false)?
    };
    // Only the consumer of the exec holds its stdio, the test holds none.
    let io = Arc::downgrade(&registered.io);
    let mut exit_rx = reaper.watch_grandchild(registered, InFlight::default())?;
    let exit = exit_rx.recv().await?;
    assert_eq!(exit.exit_code, EXIT_CODE_TIMED_OUT);
    assert!(exit.timed_out);

    // Covers the window between the exit, which goes out at the timeout,
    // and the reap of the killed process, which one stuck in the kernel
    // would not leave. The current thread runtime runs this test before the
    // watcher gets polled again, which waits for the reactor, so nothing
    // below yields to it before the checks. The orphan sweep leaves the
    // process to the watcher, and the shutdown reports it. Its pipes or its
    // terminal do not stay open meanwhile.
    //
    // This relies on the pidfd of the process. Without one (Linux before
    // 5.3), a thread of its own reaps the process and wakes the watcher from
    // outside the runtime, which may poll the watcher before this test. The
    // watcher may forget the exec then, and the claim and the report fail.
    // The released stdio does not depend on it. CI kernels have pidfds.
    assert!(reaper.is_claimed(exec.pid));
    assert!(io.upgrade().is_none());
    let err = reaper
        .kill_grandchildren(Instant::now())
        .await
        .err()
        .map(|e| format!("{e:#}"));
    assert_eq!(err, Some(format!("PIDs [{}] did not exit", exec.pid)));
    // And waits for it until it got reaped.
    reaper
        .kill_grandchildren(Instant::now() + Duration::from_secs(10))
        .await?;
    assert!(is_gone(exec.pid));
    Ok(())
}

#[tokio::test]
async fn terminate_signals_the_containers_only() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let container = spawn_sh("exec sleep 60")?;
    let registered = child(container.pid, None, &dir.path().join("container"))?;
    reaper.watch_grandchild(registered, InFlight::default())?;
    let exec = spawn_sh("exec sleep 60")?;
    let registered = Child {
        kind: ChildKind::Exec,
        ..child(exec.pid, None, &dir.path().join("exec"))?
    };
    reaper.watch_grandchild(registered, InFlight::default())?;

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut terminate = pin!(reaper.terminate_containers(deadline));
    // Signals on the first poll, the watchers did not run yet.
    assert!((&mut terminate).now_or_never().is_none());
    // A container registered later gets neither signalled nor waited for.
    let late = spawn_sh("exec sleep 60")?;
    let registered = Child {
        id: "late".into(),
        ..child(late.pid, None, &dir.path().join("late"))?
    };
    reaper.watch_grandchild(registered, InFlight::default())?;
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
    let registered = child(ignoring.pid, None, &dir.path().join("container"))?;
    reaper.watch_grandchild(registered, InFlight::default())?;

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
    let registered = child(running.pid, None, &dir.path().join("exit"))?;
    reaper.watch_grandchild(registered, call)?;

    // A call in progress registers its child after the snapshot.
    let call = reaper.begin_call(distant_deadline())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let shutdown = task::spawn({
        let reaper = reaper.clone();
        async move { reaper.kill_grandchildren(deadline).await }
    });
    wait_for_real(|| is_gone(running.pid)).await;
    let late = spawn_sh("exec sleep 60")?;
    let registered = child(late.pid, None, &dir.path().join("late"))?;
    let err = reaper.watch_grandchild(registered, call).err();
    assert_eq!(
        err.map(|e| format!("{e:#}")).as_deref(),
        Some("server is shutting down")
    );

    time::timeout(Duration::from_secs(20), shutdown).await???;
    // The container registered before the snapshot still gets its exit
    // file, which CRI-O reads after a restart.
    let killed = 128 + Signal::SIGKILL as i32;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("exit"))?,
        killed.to_string()
    );
    assert!(is_gone(late.pid));
    // Its create failed, so it leaves no exit file.
    assert!(!dir.path().join("late").exists());
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
    let (id, exited, _) = ReapableChild::from_child(child(pid, None, &dir.path().join("exit"))?);
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
