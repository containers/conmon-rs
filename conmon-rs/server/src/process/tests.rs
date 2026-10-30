use super::*;
use crate::test_util::Guarded;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use tokio::time::{self, Duration};

#[test]
fn stat_follows_the_command_name() {
    assert_eq!(
        Stat::parse(b"42 (a) b) (c) Z 1 42 42 0 -1"),
        Some(Stat {
            state: 'Z',
            ppid: 1,
            pgrp: 42
        })
    );
    assert_eq!(Stat::parse(b"42 (a) R"), None);
    // Any process can give itself a name which is not UTF-8.
    assert_eq!(
        Stat::parse(b"42 (\xff\xfe) Z 1 7 7 0 -1"),
        Some(Stat {
            state: 'Z',
            ppid: 1,
            pgrp: 7
        })
    );
}

#[test]
fn pid_files_hold_positive_pids() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("pid");
    for (content, pid) in [
        ("42", Some(42)),
        ("42\n", Some(42)),
        ("0", None),
        ("-1", None),
        ("x", None),
        ("", None),
    ] {
        std::fs::write(&path, content)?;
        assert_eq!(
            read_pid_file(&path).ok(),
            pid.map(Pid::from_raw),
            "{content:?}"
        );
    }
    assert!(read_pid_file(&dir.path().join("missing")).is_err());
    Ok(())
}

#[test]
fn child_ids_are_positive_pids() {
    let pid = |id: Option<u32>| child_pid(id).ok().map(Pid::as_raw);
    assert_eq!(pid(Some(42)), Some(42));
    assert_eq!(child_pid(42).ok(), Some(Pid::from_raw(42)));
    assert_eq!(pid(Some(i32::MAX as u32)), Some(i32::MAX));
    // A reaped child has none, zero and what does not fit would refer
    // to process groups.
    assert_eq!(pid(None), None);
    assert_eq!(pid(Some(0)), None);
    assert_eq!(pid(Some(i32::MAX as u32 + 1)), None);
    assert_eq!(pid(Some(u32::MAX)), None);
    for id in [None, Some(0), Some(u32::MAX)] {
        let err = child_pid(id).err().map(|e| format!("{e:#}"));
        assert!(err.is_some_and(|e| !e.is_empty()));
    }
}

#[test]
fn children_are_found_by_scan() -> Result<()> {
    let mut child = Guarded::spawn(std::process::Command::new("sleep").arg("60"))?;
    let pid = child_pid(child.0.id())?;
    let found = children_by_scan().contains(&pid);
    let zombie = is_zombie(pid);
    child.0.kill()?;
    child.0.wait()?;
    assert!(found);
    assert!(!zombie);
    Ok(())
}

#[test]
fn own_process_group_is_not_signalled() -> Result<()> {
    let own = getpgid(None)?;
    assert!(!signals_group(own, Some(own)));
    assert!(!signals_group(Pid::from_raw(1), Some(own)));
    assert!(!signals_group(Pid::from_raw(0), None));
    assert!(signals_group(Pid::from_raw(own.as_raw() + 1), Some(own)));
    Ok(())
}

#[test]
fn signal_group_reaches_the_group_until_reaped() -> Result<()> {
    for use_pidfd in [true, false] {
        // A shell in a group of its own with a child in the same group.
        let mut child = Guarded::spawn(
            std::process::Command::new("sh")
                .args(["-c", "sleep 60 & wait"])
                .process_group(0),
        )?;
        let pid = child_pid(child.0.id())?;
        let process = if use_pidfd {
            Process::open(pid)
        } else {
            Process::child(pid)
        };

        process.signal_group(Signal::SIGTERM)?;
        assert_eq!(child.0.wait()?.signal(), Some(Signal::SIGTERM as i32));
        // Its group is gone with it, nothing gets signalled anymore.
        assert!(process.signal_group(Signal::SIGTERM).is_err());
    }
    Ok(())
}

#[test]
fn signal_group_reaches_members_of_reaped_leader() -> Result<()> {
    // The leader of a group, and a member which outlives it.
    let mut leader = Guarded::spawn(
        std::process::Command::new("sleep")
            .arg("60")
            .process_group(0),
    )?;
    let pgid = child_pid(leader.0.id())?;
    let mut member = Guarded::spawn(
        std::process::Command::new("sleep")
            .arg("60")
            .process_group(pgid.as_raw()),
    )?;
    leader.0.kill()?;
    leader.0.wait()?;

    let process = Process::child(pgid);
    process.signal_group(Signal::SIGTERM)?;
    assert_eq!(member.0.wait()?.signal(), Some(Signal::SIGTERM as i32));

    // Without members, nothing gets signalled.
    assert!(process.signal_group(Signal::SIGTERM).is_err());
    Ok(())
}

#[test]
fn process_signal_reaches_child() -> Result<()> {
    for use_pidfd in [true, false] {
        let mut child = Guarded::spawn(std::process::Command::new("sleep").arg("60"))?;
        let pid = child_pid(child.0.id())?;
        let mut process = Process::open(pid);
        if !use_pidfd {
            process.pidfd = None;
        }

        process.signal(Signal::SIGTERM)?;
        assert_eq!(child.0.wait()?.signal(), Some(Signal::SIGTERM as i32));
    }
    Ok(())
}

#[test]
fn process_signal_skips_reaped_child() -> Result<()> {
    let mut child = std::process::Command::new("true").spawn()?;
    let pid = child_pid(child.id())?;
    let mut process = Process::open(pid);
    assert!(process.pidfd.is_some());
    child.wait()?;

    // The PID may belong to another process by now, neither way signals it.
    assert_eq!(process.signal(Signal::SIGTERM), Err(Errno::ESRCH));
    process.pidfd = None;
    assert_eq!(process.signal(Signal::SIGTERM), Err(Errno::ECHILD));
    Ok(())
}

#[tokio::test]
async fn wait_for_reaped_process_is_unknown() -> Result<()> {
    let mut reaped = std::process::Command::new("true").spawn()?;
    let pid = child_pid(reaped.id())?;
    reaped.wait()?;
    let code = time::timeout(Duration::from_secs(5), wait_for_exit_code(pid)).await?;
    assert_eq!(code, EXIT_CODE_UNKNOWN);
    Ok(())
}

/// Spawn a shell running `script` which is never waited for by the standard
/// library, the tests reap it themselves.
fn spawn_unwaited(script: &str) -> Result<Pid> {
    let child = std::process::Command::new("sh")
        .args(["-c", script])
        .spawn()?;
    let pid = child_pid(child.id())?;
    std::mem::forget(child);
    Ok(pid)
}

/// The first real-time signal of glibc, which nix does not know. The tests of
/// its exits run on their own: qemu-user, which keeps some real-time signals
/// for itself, reports them with other numbers, see the test-unit-arch jobs.
const RT_SIGNAL: i32 = 34;

#[test]
fn wait_exit_reports_exit_codes() -> Result<()> {
    check_wait_exit(&[("exit 3", 3), ("kill -9 $$", 128 + Signal::SIGKILL as i32)])
}

#[test]
fn wait_exit_reports_real_time_signal_exits() -> Result<()> {
    check_wait_exit(&[(&format!("kill -{RT_SIGNAL} $$"), 128 + RT_SIGNAL)])
}

fn check_wait_exit(cases: &[(&str, i32)]) -> Result<()> {
    for &(script, expected) in cases {
        let pid = spawn_unwaited(script)?;
        // Without reaping it, which happens afterwards.
        assert_eq!(
            wait_exit(Id::Pid(pid), WaitPidFlag::WNOWAIT),
            Ok(Some(expected)),
            "{script}"
        );
        assert!(is_zombie(pid), "{script}");
        assert_eq!(peek_exit(Id::Pid(pid)), Ok(Some(expected)), "{script}");
        assert_eq!(
            wait_exit(Id::Pid(pid), WaitPidFlag::WNOHANG),
            Ok(Some(expected)),
            "{script}"
        );
        assert_eq!(peek_exit(Id::Pid(pid)), Err(Errno::ECHILD), "{script}");
    }
    Ok(())
}

#[test]
fn wait_exit_reports_running_child() -> Result<()> {
    let mut child = Guarded::spawn(std::process::Command::new("sleep").arg("60"))?;
    let pid = child_pid(child.0.id())?;
    assert_eq!(wait_exit(Id::Pid(pid), WaitPidFlag::WNOHANG), Ok(None));
    assert_eq!(peek_exit(Id::Pid(pid)), Ok(None));
    child.0.kill()?;
    child.0.wait()?;
    Ok(())
}

#[tokio::test]
async fn wait_on_thread_reports_exit_codes() -> Result<()> {
    check_wait_on_thread(&[
        ("exit 3", 3),
        ("kill -9 $$", 128 + Signal::SIGKILL as i32),
        // Killed while the thread waits for it.
        ("exec sleep 60", 128 + Signal::SIGKILL as i32),
    ])
    .await
}

#[tokio::test]
async fn wait_on_thread_reports_real_time_signal_exits() -> Result<()> {
    check_wait_on_thread(&[(&format!("kill -{RT_SIGNAL} $$"), 128 + RT_SIGNAL)]).await
}

async fn check_wait_on_thread(cases: &[(&str, i32)]) -> Result<()> {
    for &(script, expected) in cases {
        let pid = spawn_unwaited(script)?;
        let process = Process::open(pid);
        let wait = task::spawn(wait_for_exit_code_on_thread(pid));
        if script.contains("sleep") {
            process.signal(Signal::SIGKILL)?;
        }
        let code = time::timeout(Duration::from_secs(5), wait).await??;
        assert_eq!(code, expected, "{script}");
        // It got reaped.
        assert_eq!(peek_exit(Id::Pid(pid)), Err(Errno::ECHILD), "{script}");
    }
    Ok(())
}

#[tokio::test]
async fn wait_on_thread_for_reaped_process_fails() -> Result<()> {
    let mut reaped = std::process::Command::new("true").spawn()?;
    let pid = child_pid(reaped.id())?;
    reaped.wait()?;
    let code = time::timeout(Duration::from_secs(5), wait_for_exit_code_on_thread(pid)).await?;
    // The same value as a timeout, for compatibility.
    assert_eq!(code, EXIT_CODE_WAIT_FAILED);
    Ok(())
}
