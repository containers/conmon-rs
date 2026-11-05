//! Running a runtime for a call and the cleanup after a failure.

use super::*;

#[tokio::test]
async fn failed_create_cleanup_spares_claimed_pid() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let pidfile = dir.path().join("pid");
    let sh = spawn_sh("exec sleep 60")?;
    let pid = sh.pid;
    std::fs::write(&pidfile, pid.to_string())?;

    // A stale PID file naming a process this server waits for.
    let pending = PendingPid::new(&reaper.pending, pid);
    reaper.cleanup_failed_grandchild(&pidfile);
    assert!(!is_gone(pid) && !is_zombie(pid));

    drop(pending);
    reaper.cleanup_failed_grandchild(&pidfile);
    wait_for_real(|| is_gone(pid)).await;
    Ok(())
}

#[tokio::test]
async fn failed_call_cleanup_kills_the_group() -> Result<()> {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let pidfile = dir.path().join("pid");
    // The process of a failed call, which gets reaped by the cleanup, and a
    // worker it started in its group.
    let pid = child_pid(
        Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()?
            .id(),
    )?;
    let _leader = Spawned {
        pid,
        process: Process::open(pid),
    };
    let mut worker = Guarded::spawn(Command::new("sleep").arg("60").process_group(pid.as_raw()))?;
    std::fs::write(&pidfile, pid.to_string())?;

    reaper.cleanup_failed_grandchild(&pidfile);
    let worker_pid = child_pid(worker.0.id())?;
    wait_for_real(|| is_zombie(worker_pid)).await;
    assert_eq!(worker.0.wait()?.signal(), Some(Signal::SIGKILL as i32));
    wait_for_real(|| is_gone(pid)).await;
    Ok(())
}

#[tokio::test]
async fn abandoned_call_releases_its_process() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let pidfile = dir.path().join("pid");
    let script = format!("sleep 60 & echo $! > {}", pidfile.display());
    let args = ["-c".into(), script];
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let (pid, token, call) = reaper
        .create_child(
            RuntimeCall::new(Path::new("sh"), &args, &pidfile, distant_deadline()),
            &mut io,
        )
        .await?;
    // Not a child of the tests, which are no subreaper.
    let process = Process::open(pid);
    token.cancel();
    assert!(reaper.is_claimed(pid));
    assert!(reaper.calls_in_progress());

    // A caller which gives up before the registration, or whose future
    // gets dropped, does not leave the process claimed.
    drop(call);
    assert!(!reaper.is_claimed(pid));
    assert!(!reaper.calls_in_progress());
    process.signal(Signal::SIGKILL)?;
    Ok(())
}

#[tokio::test]
async fn runtime_without_stdin_gets_dev_null() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let (pidfile, stdin) = (dir.path().join("pid"), dir.path().join("stdin"));
    let script = format!(
        "readlink /proc/self/fd/0 > {}; echo $$ > {}",
        stdin.display(),
        pidfile.display()
    );
    let args = ["-c".into(), script];
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let (_, token, _call) = reaper
        .create_child(
            RuntimeCall::new(Path::new("sh"), &args, &pidfile, distant_deadline()),
            &mut io,
        )
        .await?;
    token.cancel();
    assert_eq!(std::fs::read_to_string(stdin)?.trim(), "/dev/null");
    Ok(())
}

#[tokio::test]
async fn runtime_output_with_terminal_does_not_block() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let mut io = ContainerIO::new(true, ContainerLog::new())?;

    // More than a pipe holds, the error comes last.
    let script = "head -c 300000 /dev/zero | tr '\\0' x; head -c 300000 /dev/zero >&2; \
                  echo the-error >&2; exit 1";
    let start = Instant::now();
    let pidfile = dir.path().join("pid");
    let args = ["-c".into(), script.into()];
    let call = RuntimeCall::new(
        Path::new("sh"),
        &args,
        &pidfile,
        Instant::now() + Duration::from_secs(10),
    );
    let res = time::timeout(Duration::from_secs(20), reaper.create_child(call, &mut io)).await?;
    let err = format!("{:#}", res.err().context("runtime failure expected")?);
    assert!(err.contains("runtime exited with status 1"), "{err}");
    assert!(err.trim_end().ends_with("the-error"));
    assert!(err.len() < 2 * RuntimeOutput::MAX_STDERR);
    assert!(start.elapsed() < Duration::from_secs(10));
    Ok(())
}

/// A take gets what is still in the pipe within its wait, and gives up on a
/// pipe which stays open without blocking for longer.
#[tokio::test]
async fn taken_output_gets_the_rest_within_its_wait() -> Result<()> {
    let mut child = tokio::process::Command::new("sh")
        .args(["-c", "printf a >&2; sleep 0.2; printf b >&2"])
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let output = RuntimeOutput::spawn(None, child.stderr.take());
    assert_eq!(output.take(Duration::from_secs(10)).await, b"ab");

    let mut child = tokio::process::Command::new("sh")
        .args(["-c", "printf a >&2; exec sleep 60"])
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let output = RuntimeOutput::spawn(None, child.stderr.take());
    wait_for(|| output.has_stderr()).await?;
    let start = Instant::now();
    assert_eq!(output.take(Duration::from_millis(100)).await, b"a");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    Ok(())
}

#[tokio::test]
async fn runtime_killed_by_a_signal_names_it() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let pidfile = dir.path().join("pid");
    let args = ["-c".into(), "kill -KILL $$".into()];
    let call = RuntimeCall::new(
        Path::new("sh"),
        &args,
        &pidfile,
        Instant::now() + Duration::from_secs(10),
    );
    let res = time::timeout(Duration::from_secs(20), reaper.create_child(call, &mut io)).await?;
    let err = format!("{:#}", res.err().context("runtime failure expected")?);
    assert!(
        err.contains("runtime got killed by signal SIGKILL"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn hanging_runtime_gets_killed_at_its_deadline() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let pidfile = dir.path().join("pid");
    let args = ["-c".into(), "exec sleep 60".into()];
    for terminal in [false, true] {
        let mut io = ContainerIO::new(terminal, ContainerLog::new())?;
        let call = RuntimeCall::new(
            Path::new("sh"),
            &args,
            &pidfile,
            Instant::now() + Duration::from_millis(200),
        );
        let start = Instant::now();
        let res =
            time::timeout(Duration::from_secs(20), reaper.create_child(call, &mut io)).await?;
        let err = format!("{:#}", res.err().context("runtime failure expected")?);
        assert!(err.contains("did not finish in time"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(10));
        // The call ended with it.
        assert!(!reaper.calls_in_progress());
    }
    Ok(())
}

#[tokio::test]
async fn hanging_runtime_gets_killed_with_its_group() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let run = async |terminal: bool| -> Result<()> {
        let pidfile = dir.path().join(format!("pid-{terminal}"));
        let helper_pidfile = dir.path().join(format!("helper-{terminal}"));
        // Like a runc init which the runtime started before it hung,
        // which holds the pipes of the runtime as well.
        let script = format!(
            "sleep 60 & echo $! > {}; exec sleep 60",
            helper_pidfile.display()
        );
        let args = ["-c".into(), script];
        let mut io = ContainerIO::new(terminal, ContainerLog::new())?;
        let call = RuntimeCall::new(
            Path::new("sh"),
            &args,
            &pidfile,
            Instant::now() + Duration::from_secs(1),
        );
        // Its output does not wait for the helper either, which runs
        // for a minute.
        let res =
            time::timeout(Duration::from_secs(20), reaper.create_child(call, &mut io)).await?;
        let err = format!("{:#}", res.err().context("runtime failure expected")?);
        assert!(err.contains("did not finish in time"), "{err}");
        // Reparented to the init or a subreaper above the tests.
        let helper = read_pid_file(&helper_pidfile)?;
        wait_for_real(|| is_gone(helper) || is_zombie(helper)).await;
        Ok(())
    };
    let (streams, terminal) = tokio::join!(run(false), run(true));
    streams.and(terminal)
}

#[tokio::test]
async fn stale_pid_file_does_not_get_cleaned_up() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let pidfile = dir.path().join("pid");
    // A child of the server which nobody registered, like the pause init.
    let other = spawn_sh("exec sleep 60")?;
    std::fs::write(&pidfile, other.pid.to_string())?;

    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let args = ["-c".into(), "exit 1".into()];
    let call = RuntimeCall::new(Path::new("sh"), &args, &pidfile, distant_deadline());
    assert!(reaper.create_child(call, &mut io).await.is_err());
    assert!(!pidfile.exists());
    // A cleanup would have sent its SIGKILL before the call returned. A
    // signal sent now comes second, the exit tells which one ended it.
    other.process.signal(Signal::SIGTERM)?;
    assert_eq!(
        wait_for_exit_code(other.pid).await,
        128 + Signal::SIGTERM as i32
    );
    Ok(())
}

#[tokio::test]
async fn dropped_runtime_gets_killed_with_its_group() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let pidfile = dir.path().join("pid");
    let helper_pidfile = dir.path().join("helper");
    let script = format!(
        "sleep 60 & echo $! > {}; exec sleep 60",
        helper_pidfile.display()
    );
    let args = ["-c".into(), script];
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let call = RuntimeCall::new(Path::new("sh"), &args, &pidfile, distant_deadline());
    // Like a call in progress which the shutdown drops.
    select! {
        _ = reaper.create_child(call, &mut io) => bail!("runtime finished"),
        () = wait_for_real(|| read_pid_file(&helper_pidfile).is_ok()) => {}
    }
    let helper = read_pid_file(&helper_pidfile)?;
    wait_for_real(|| is_gone(helper) || is_zombie(helper)).await;
    Ok(())
}

/// A spawn which failed because a target of the passed descriptors changed
/// gets tried once more. The passed descriptor and its duplicates get closed
/// either way.
#[tokio::test]
async fn spawns_with_changed_targets_get_retried_once() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let pidfile = dir.path().join("pid");
    let args = ["-c".into(), "exit 1".into()];
    for (changes, retried) in [(1, true), (2, false)] {
        // Reading it ends once all copies of the other end got closed.
        let (passed, reader) = std::os::unix::net::UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        runtime_call::TARGET_CHANGES.with(|n| n.set(changes));
        let mut io = ContainerIO::new(false, ContainerLog::new())?;
        let call = RuntimeCall {
            additional_fds: vec![passed.into()],
            ..RuntimeCall::new(Path::new("sh"), &args, &pidfile, distant_deadline())
        };
        let err = reaper.create_child(call, &mut io).await.unwrap_err();
        runtime_call::TARGET_CHANGES.with(|n| n.set(0));
        // The runtime ran and failed on its own.
        assert_eq!(
            !format!("{err:#}").contains("a target descriptor changed during spawn"),
            retried,
            "{err:#}"
        );
        // A process which a parallel test forks may hold a copy until it
        // execs.
        wait_for_real(|| matches!(std::io::Read::read(&mut &reader, &mut [0; 1]), Ok(0))).await;
    }
    Ok(())
}
