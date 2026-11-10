//! Running a runtime for a call and the cleanup after a failure.

use super::*;
use crate::util::format_duration;

#[tokio::test]
async fn failed_create_cleanup_spares_claimed_pid() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let pidfile = dir.path().join("pid");
    let sh = spawn_sh("exec sleep 60")?;
    let pid = sh.pid;
    std::fs::write(&pidfile, pid.to_string())?;

    // A stale PID file naming a process this server waits for. A cleanup
    // would have sent its SIGKILL before it returned. A signal sent now
    // comes second, the exit tells which one ended it.
    let pending = PendingPid::new(&reaper.pending, pid);
    reaper.cleanup_failed_grandchild(&pidfile);
    sh.process.signal(Signal::SIGTERM)?;
    assert_eq!(wait_for_exit_code(pid).await, 128 + Signal::SIGTERM as i32);
    drop(pending);

    // Once nothing waits for it, it gets cleaned up.
    let sh = spawn_sh("exec sleep 60")?;
    let pid = sh.pid;
    std::fs::write(&pidfile, pid.to_string())?;
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
    let StartedChild { pid, token, call } = reaper
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
    let StartedChild {
        token, call: _call, ..
    } = reaper
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
    assert!(err.len() < 2 * STDERR_TAIL_SIZE);
    assert!(start.elapsed() < Duration::from_secs(10));
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
        assert!(err.contains("did not finish within"), "{err}");
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
        assert!(err.contains("did not finish within"), "{err}");
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
    // A child of the server which nobody registered.
    let other = spawn_sh("exec sleep 60")?;
    std::fs::write(&pidfile, other.pid.to_string())?;

    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let args = ["-c".into(), "exit 1".into()];
    let call = RuntimeCall::new(Path::new("sh"), &args, &pidfile, distant_deadline());
    let err = reaper.create_child(call, &mut io).await.err();
    assert_eq!(
        err.map(|e| format!("{e:#}")).as_deref(),
        Some("runtime exited with status 1")
    );
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

#[test]
fn runtime_timeouts_get_formatted() {
    for (secs, expected) in [
        (600, "10m"),
        (90, "1m 30s"),
        (60, "1m"),
        (45, "45s"),
        (0, "0s"),
    ] {
        assert_eq!(format_duration(Duration::from_secs(secs)), expected);
    }
    // A deadline measured from now is short of a whole second.
    assert_eq!(format_duration(Duration::from_millis(59_900)), "1m");
}

#[tokio::test]
async fn runtime_with_terminal_gets_no_stdin_pipe() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let (pidfile, stdin) = (dir.path().join("pid"), dir.path().join("stdin"));
    // The process gets the terminal as its stdin, the runtime none. The
    // runtime fails, so the call does not wait for the terminal.
    let script = format!("readlink /proc/self/fd/0 > {}; exit 1", stdin.display());
    let args = ["-c".into(), script];
    let mut io = ContainerIO::new(true, ContainerLog::new())?;
    let call = RuntimeCall {
        stdin: true,
        ..RuntimeCall::new(Path::new("sh"), &args, &pidfile, distant_deadline())
    };
    let res = time::timeout(Duration::from_secs(20), reaper.create_child(call, &mut io)).await?;
    let err = format!("{:#}", res.err().context("runtime failure expected")?);
    assert!(err.contains("runtime exited with status 1"), "{err}");
    assert_eq!(std::fs::read_to_string(stdin)?.trim(), "/dev/null");
    Ok(())
}

/// The tests are no subreaper, so the processes of their runtimes are no
/// children of theirs, see `abandoned_call_releases_its_process`. The check
/// of the server gets its flag.
#[test]
fn reported_pids_have_to_be_children() -> Result<()> {
    let mut child = Guarded::spawn(Command::new("sleep").arg("60"))?;
    let pid = child_pid(child.0.id())?;
    ChildReaper::check_reported_pid(pid, true)?;

    // Like a runtime which names a process of somebody else, which must not
    // get registered and signalled later.
    let foreign = Pid::from_raw(1);
    let err = ChildReaper::check_reported_pid(foreign, true)
        .err()
        .map(|e| format!("{e:#}"));
    assert_eq!(
        err.as_deref(),
        Some("runtime reported PID 1, which is not a child")
    );
    // A server without the flag never gets the processes as children.
    ChildReaper::check_reported_pid(foreign, false)?;

    // A child which exited is still one until it got reaped.
    child.0.kill()?;
    wait_for_real_blocking(|| is_zombie(pid));
    ChildReaper::check_reported_pid(pid, true)?;
    child.0.wait()?;
    assert!(ChildReaper::check_reported_pid(pid, true).is_err());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn exec_runtimes_always_get_a_deadline() {
    let now = Instant::now();
    let timeout = now + Duration::from_secs(10);
    assert_eq!(
        ChildReaper::exec_runtime_deadline(Some(timeout)),
        timeout + ChildReaper::EXEC_RUNTIME_GRACE
    );
    // Without a timeout of the process, the runtime still gets one.
    assert_eq!(
        ChildReaper::exec_runtime_deadline(None),
        now + ChildReaper::EXEC_RUNTIME_GRACE
    );
}
