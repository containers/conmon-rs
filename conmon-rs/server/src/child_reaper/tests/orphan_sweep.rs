//! Sweeping unclaimed zombies, and the ones a pending call may still claim.

use super::*;

/// Spawn a child which exits right away with `code` and is never waited
/// for.
fn spawn_zombie(code: i32) -> Result<Pid> {
    let child = Command::new("sh")
        .args(["-c", &format!("exit {code}")])
        .spawn()?;
    let pid = child_pid(child.id())?;
    std::mem::forget(child);
    let start = Instant::now();
    while !is_zombie(pid) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "child never became a zombie"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(pid)
}

/// Set for the test process which runs the sweep test.
const SWEEP_TEST_ENV: &str = "CONMONRS_SWEEP_TEST";

/// The sweep reaps every unclaimed zombie of the test process, so it would
/// take the children of parallel tests, and their zombies would make it
/// flaky. Run it in a process of its own.
#[test]
fn sweep_reaps_unclaimed_zombies_after_grace() -> Result<()> {
    let output = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "child_reaper::tests::orphan_sweep::sweep_in_own_process",
            "--ignored",
        ])
        .env(SWEEP_TEST_ENV, "1")
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("1 passed"), "{stdout}");
    Ok(())
}

#[tokio::test]
#[ignore = "run by sweep_reaps_unclaimed_zombies_after_grace in a process of its own"]
async fn sweep_in_own_process() -> Result<()> {
    if std::env::var_os(SWEEP_TEST_ENV).is_none() {
        return Ok(());
    }
    let reaper = ChildReaper::default();
    let (grace, max_defer) = (Duration::ZERO, Duration::from_secs(60));
    assert!(!has_zombie());
    let orphan = spawn_zombie(0)?;
    let claimed = spawn_zombie(0)?;
    let _claimed = PendingPid::new(&reaper.pending, claimed);
    assert!(has_zombie());

    // Both ways of listing the children find them.
    assert!(zombie_children().0.contains(&orphan));
    assert!(children_by_scan().contains(&orphan));

    // The first sweep only records the unclaimed one as candidate.
    let mut candidates = HashMap::new();
    reaper
        .sweep_orphans(&mut candidates, Duration::from_secs(60), max_defer)
        .await;
    assert!(is_zombie(orphan));
    assert!(candidates.contains_key(&orphan));
    assert!(!candidates.contains_key(&claimed));

    // Without grace the unclaimed one is reaped, the claimed one stays.
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(!is_zombie(orphan));
    assert!(is_zombie(claimed));
    assert!(!candidates.contains_key(&orphan));

    // The process of a call in progress which exited before the call
    // claimed it, like an exec which ends before its runtime exited,
    // stays for the call, which gets its exit code.
    let call = reaper.begin_call(distant_deadline())?;
    let exec = spawn_zombie(3)?;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(is_zombie(exec));
    assert_eq!(wait_for_exit_code(exec).await, 3);
    drop(call);

    // A call which hangs keeps the sweep from reaping only up to the
    // bound.
    let call = reaper.begin_call(distant_deadline())?;
    let late = spawn_zombie(0)?;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(is_zombie(late));
    reaper
        .sweep_orphans(&mut candidates, grace, Duration::ZERO)
        .await;
    assert!(!is_zombie(late));
    drop(call);

    // A call which cannot claim a process anymore defers none, see
    // `deferral_ends_with_the_call_or_the_bound`.
    let past = Instant::now()
        .checked_sub(ChildReaper::CLAIM_MARGIN + Duration::from_secs(1))
        .context("past deadline")?;
    let call = reaper.begin_call(past)?;
    let past_its_deadline = spawn_zombie(0)?;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(!is_zombie(past_its_deadline));
    drop(call);

    // Calls which started after the zombie was first seen cannot claim
    // it, they do not defer it, neither after the calls it was seen with
    // ended nor without any.
    let call = reaper.begin_call(distant_deadline())?;
    let first = spawn_zombie(0)?;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(is_zombie(first));
    drop(call);
    let unrelated = reaper.begin_call(distant_deadline())?;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(!is_zombie(first));
    drop(unrelated);

    let second = spawn_zombie(0)?;
    reaper
        .sweep_orphans(&mut candidates, Duration::from_secs(60), max_defer)
        .await;
    assert!(candidates.contains_key(&second));
    let _in_progress = reaper.begin_call(distant_deadline())?;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(!is_zombie(second));

    // Once the server shuts down, the calls in progress cannot register
    // a process anymore and do not defer any, like the one above which
    // started before this zombie.
    let shutting_down = spawn_zombie(0)?;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(is_zombie(shutting_down));
    reaper.shutting_down.store(true, Ordering::SeqCst);
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(!is_zombie(shutting_down));
    reaper.shutting_down.store(false, Ordering::SeqCst);

    waitpid(claimed, None)?;
    assert!(!zombie_children().0.contains(&claimed));
    assert!(!has_zombie());

    // Without zombies the sweep forgets its candidates.
    candidates.insert(
        claimed,
        Candidate {
            first_seen: Instant::now(),
            calls_before: 0,
        },
    );
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(candidates.is_empty());
    Ok(())
}

#[test]
fn pending_marks_of_a_reused_pid_are_counted() {
    let reaper = ChildReaper::default();
    let pid = Pid::from_raw(1234);
    // The runtime of one call got reaped, its PID went to the process of
    // another call, before the first call ended.
    let runtime = PendingPid::new(&reaper.pending, pid);
    let process = PendingPid::new(&reaper.pending, pid);
    drop(runtime);
    assert!(reaper.is_claimed(pid));
    drop(process);
    assert!(!reaper.is_claimed(pid));
    assert!(lock(&reaper.pending).is_empty());
}

/// The exit of an exec whose runtime is slow, but within its deadline,
/// stays for the call, a hung create does not defer orphans for longer
/// than the bound.
#[tokio::test(start_paused = true)]
async fn deferral_ends_with_the_call_or_the_bound() -> Result<()> {
    let reaper = ChildReaper::default();
    let max_defer = ChildReaper::ORPHAN_MAX_DEFER;
    let first_seen = |reaper: &ChildReaper| Candidate {
        first_seen: Instant::now(),
        calls_before: reaper.in_flight.borrow().next,
    };

    // The runtime of an exec gets a minute, its process exits after
    // 29.5s and the runtime after 31s.
    let exec = reaper.begin_call(Instant::now() + Duration::from_secs(60))?;
    time::advance(Duration::from_millis(29_500)).await;
    let zombie = first_seen(&reaper);
    time::advance(Duration::from_millis(1500)).await;
    assert!(reaper.defers(zombie, Instant::now(), max_defer));
    // Not past the bound since it was first seen.
    time::advance(max_defer - Duration::from_secs(2)).await;
    assert!(reaper.defers(zombie, Instant::now(), max_defer));
    time::advance(Duration::from_secs(2)).await;
    assert!(!reaper.defers(zombie, Instant::now(), max_defer));

    // Not past the deadline of the call and its margin either.
    let late = first_seen(&reaper);
    time::advance(ChildReaper::CLAIM_MARGIN - Duration::from_millis(1500)).await;
    assert!(reaper.defers(late, Instant::now(), max_defer));
    time::advance(Duration::from_secs(1)).await;
    assert!(!reaper.defers(late, Instant::now(), max_defer));
    drop(exec);

    // A create gets 10 minutes, the bound since the zombie was first
    // seen ends the deferral first, also late in the call.
    let create = reaper.begin_call(Instant::now() + Duration::from_secs(600))?;
    for after in [Duration::from_secs(1), Duration::from_secs(500)] {
        time::advance(after).await;
        let zombie = first_seen(&reaper);
        time::advance(max_defer - Duration::from_millis(1)).await;
        assert!(reaper.defers(zombie, Instant::now(), max_defer));
        time::advance(Duration::from_millis(1)).await;
        assert!(!reaper.defers(zombie, Instant::now(), max_defer));
    }

    // An older call with a later deadline still claims, a call which
    // started after the zombie was first seen does not.
    let exec = reaper.begin_call(Instant::now() + Duration::from_secs(1))?;
    let zombie = first_seen(&reaper);
    let unrelated = reaper.begin_call(Instant::now() + Duration::from_secs(60))?;
    time::advance(Duration::from_secs(20)).await;
    assert!(reaper.defers(zombie, Instant::now(), max_defer));
    drop(create);
    assert!(!reaper.defers(zombie, Instant::now(), max_defer));
    drop((exec, unrelated));
    Ok(())
}
