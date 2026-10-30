use super::*;
use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, DRAIN_MAX_LOG_WAIT, DRAIN_MAX_WAIT, DRAIN_TIMEOUT, Message},
    container_log::ContainerLog,
    oom_watcher::OOMEvent,
    process::{
        Process, child_pid, children_by_scan, has_zombie, is_zombie, read_pid_file,
        wait_for_exit_code, zombie_children,
    },
    test_util::wait_for_real,
};
use nix::sys::{
    signal::kill,
    wait::{WaitStatus, waitpid},
};
use std::path::Path;
use std::process::Command;
use tokio::io::AsyncWriteExt;
use tokio::{select, task};

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
            "child_reaper::tests::sweep_in_own_process",
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
    let call = reaper.begin_call(None)?;
    let exec = spawn_zombie(3)?;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(is_zombie(exec));
    let token = CancellationToken::new();
    assert_eq!(wait_for_exit_code(token.clone(), exec).await, 3);
    drop(call);

    // A call which hangs keeps the sweep from reaping only up to the
    // bound.
    let call = reaper.begin_call(None)?;
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
    let call = reaper.begin_call(Some(past))?;
    let past_its_deadline = spawn_zombie(0)?;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(!is_zombie(past_its_deadline));
    drop(call);

    // Calls which started after the zombie was first seen cannot claim
    // it, they do not defer it, neither after the calls it was seen with
    // ended nor without any.
    let call = reaper.begin_call(None)?;
    let first = spawn_zombie(0)?;
    reaper
        .sweep_orphans(&mut candidates, grace, max_defer)
        .await;
    assert!(is_zombie(first));
    drop(call);
    let unrelated = reaper.begin_call(None)?;
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
    let _in_progress = reaper.begin_call(None)?;
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

fn is_gone(pid: Pid) -> bool {
    kill(pid, None).is_err()
}

/// A shell which nobody but the watcher waits for. It gets killed through
/// a pidfd if a test fails before it ended, which cannot hit a reused PID.
struct Spawned {
    pid: Pid,
    process: Process,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.process.signal(Signal::SIGKILL);
    }
}

fn spawn_sh(script: &str) -> Result<Spawned> {
    let pid = child_pid(Command::new("sh").args(["-c", script]).spawn()?.id())?;
    // Not reaped yet, so the PID is still its own.
    let process = Process::open(pid);
    Ok(Spawned { pid, process })
}

fn child(pid: Pid, timeout: Option<Instant>, exit_path: &Path, cleanup: &[&str]) -> Result<Child> {
    Ok(Child {
        id: "id".into(),
        kind: ChildKind::Container,
        pid,
        exit_paths: vec![exit_path.into()],
        oom_exit_paths: vec![],
        timeout,
        io: SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?),
        cleanup_cmd: cleanup.iter().map(ToString::to_string).collect(),
        token: CancellationToken::new(),
    })
}

#[tokio::test(start_paused = true)]
async fn watch_reports_exit_codes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    for (script, expected) in [("exit 3", 3), ("kill -9 $$", 137), ("exit 0", 0)] {
        let exit_path = dir.path().join("exit");
        let sh = spawn_sh(script)?;
        let (_, mut reapable) = ReapableChild::from_child(child(sh.pid, None, &exit_path, &[])?);
        let mut exit_rx = reapable.watch(|| {});

        let exit = exit_rx.recv().await?;
        assert_eq!(exit.exit_code, expected, "{script}");
        assert!(!exit.timed_out);
        assert_eq!(std::fs::read_to_string(&exit_path)?, expected.to_string());
        // The token tells about the exit also without an OOM watcher.
        assert!(reapable.token().is_cancelled());
    }
    // No temporary exit file is left behind.
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

#[tokio::test]
async fn write_exit_file_replaces_at_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("id");
    ReapableChild::write_exit_file(&path, "1").await?;
    ReapableChild::write_exit_file(&path, "137").await?;
    assert_eq!(std::fs::read_to_string(&path)?, "137");
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);

    // A missing directory fails without leaving anything behind.
    let missing = dir.path().join("missing").join("id");
    assert!(ReapableChild::write_exit_file(&missing, "1").await.is_err());
    assert!(
        ReapableChild::write_exit_file(Path::new("/"), "1")
            .await
            .is_err()
    );
    Ok(())
}

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

#[tokio::test(start_paused = true)]
async fn watch_kills_on_timeout() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let exit_path = dir.path().join("exit");
    let cleaned = dir.path().join("cleaned");
    let cleanup = ["touch", cleaned.to_str().context("path")?];

    let sh = spawn_sh("exec sleep 60")?;
    let pid = sh.pid;
    let timeout = Instant::now() + Duration::from_secs(1);
    let (_, mut reapable) =
        ReapableChild::from_child(child(pid, Some(timeout), &exit_path, &cleanup)?);
    let mut exit_rx = reapable.watch(|| {});

    let exit = exit_rx.recv().await?;
    assert_eq!(exit.exit_code, EXIT_CODE_TIMED_OUT);
    assert!(exit.timed_out);
    assert_eq!(std::fs::read_to_string(&exit_path)?, "-3");
    wait_for_real(|| is_gone(pid)).await;
    wait_for_real(|| cleaned.exists()).await;
    Ok(())
}

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
async fn registration_keeps_the_child_claimed() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let sh = spawn_sh("exec sleep 60")?;
    // As `create_child` leaves it.
    let mut call = reaper.begin_call(None)?;
    call.pending = Some(PendingPid::new(&reaper.pending, sh.pid));
    assert!(reaper.is_claimed(sh.pid));

    let child = child(sh.pid, None, &dir.path().join("exit"), &[])?;
    reaper.watch_grandchild(child, vec![], call)?;
    assert!(reaper.is_claimed(sh.pid));
    assert!(!lock(&reaper.pending).contains_key(&sh.pid));
    assert!(!reaper.calls_in_progress());
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

#[tokio::test]
async fn leaked_fds_stay_open_until_the_exit() -> Result<()> {
    use nix::{fcntl::OFlag, unistd::pipe2};
    use std::io::{ErrorKind, Read};

    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let sh = spawn_sh("exec sleep 60")?;
    let (reader, writer) = pipe2(OFlag::O_NONBLOCK)?;
    let mut reader = std::fs::File::from(reader);
    let call = reaper.begin_call(None)?;
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

#[tokio::test]
async fn abandoned_call_releases_its_process() -> Result<()> {
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let pidfile = dir.path().join("pid");
    let script = format!("sleep 60 & echo $! > {}", pidfile.display());
    let args = ["-c".into(), script];
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let (pid, token, call) = reaper
        .create_child(RuntimeCall::new(Path::new("sh"), &args, &pidfile), &mut io)
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
        .create_child(RuntimeCall::new(Path::new("sh"), &args, &pidfile), &mut io)
        .await?;
    token.cancel();
    assert_eq!(std::fs::read_to_string(stdin)?.trim(), "/dev/null");
    Ok(())
}

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
            RuntimeCall::new(Path::new("true"), &args, &pidfile),
            &mut io,
        )
        .await;
    assert!(res.is_err());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn late_oom_result_after_timeout_counts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let oom_path = dir.path().join("oom");
    let paths = [oom_path.clone()];

    // The watcher still confirms an event while the timeout passes.
    let confirming = async {
        time::sleep(Duration::from_secs(1)).await;
        Some(OOMEvent::possible(true))
    };
    assert!(ReapableChild::late_oom(confirming, 137, &paths).await);
    assert!(oom_path.exists());

    // A normal exit is none, and no result at all either.
    std::fs::remove_file(&oom_path)?;
    let possible = async { Some(OOMEvent::possible(true)) };
    assert!(!ReapableChild::late_oom(possible, 0, &paths).await);
    let start = Instant::now();
    assert!(!ReapableChild::late_oom(std::future::pending(), 137, &paths).await);
    assert_eq!(start.elapsed(), ReapableChild::OOM_RESULT_WAIT);
    assert!(!oom_path.exists());
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
    let call = RuntimeCall {
        deadline: Some(Instant::now() + Duration::from_secs(10)),
        ..RuntimeCall::new(Path::new("sh"), &args, &pidfile)
    };
    let res = time::timeout(Duration::from_secs(20), reaper.create_child(call, &mut io)).await?;
    let err = format!("{:#}", res.err().context("runtime failure expected")?);
    assert!(err.contains("exited with: 1"), "{err}");
    assert!(err.trim_end().ends_with("the-error"));
    assert!(err.len() < 2 * RuntimeOutput::MAX_STDERR);
    assert!(start.elapsed() < Duration::from_secs(10));
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
        let call = RuntimeCall {
            deadline: Some(Instant::now() + Duration::from_millis(200)),
            ..RuntimeCall::new(Path::new("sh"), &args, &pidfile)
        };
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
        let call = RuntimeCall {
            deadline: Some(Instant::now() + Duration::from_secs(1)),
            ..RuntimeCall::new(Path::new("sh"), &args, &pidfile)
        };
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
    let call = RuntimeCall::new(Path::new("sh"), &args, &pidfile);
    assert!(reaper.create_child(call, &mut io).await.is_err());
    assert!(!pidfile.exists());
    // A killed process takes a moment to exit, nothing tells that no kill
    // comes.
    time::sleep(Duration::from_millis(100)).await;
    assert!(!is_gone(other.pid) && !is_zombie(other.pid));
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
    let call = RuntimeCall::new(Path::new("sh"), &args, &pidfile);
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
    let exec = reaper.begin_call(Some(Instant::now() + Duration::from_secs(60)))?;
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
    let create = reaper.begin_call(Some(Instant::now() + Duration::from_secs(600)))?;
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
    let exec = reaper.begin_call(Some(Instant::now() + Duration::from_secs(1)))?;
    let zombie = first_seen(&reaper);
    let unrelated = reaper.begin_call(Some(Instant::now() + Duration::from_secs(60)))?;
    time::advance(Duration::from_secs(20)).await;
    assert!(reaper.defers(zombie, Instant::now(), max_defer));
    drop(create);
    assert!(!reaper.defers(zombie, Instant::now(), max_defer));
    drop((exec, unrelated));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_calls_in_progress() -> Result<()> {
    let reaper = Arc::new(ChildReaper::default());
    let call = reaper.begin_call(None)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let shutdown = task::spawn({
        let reaper = reaper.clone();
        async move { reaper.kill_grandchildren(deadline).await }
    });
    time::sleep(Duration::from_secs(1)).await;
    assert!(!shutdown.is_finished());
    // No new call starts meanwhile.
    assert!(reaper.begin_call(None).is_err());

    drop(call);
    shutdown.await??;

    // A call which never ends does not keep the shutdown forever, and
    // the shutdown reports it.
    for (deadline, waited) in [
        (Duration::from_secs(60), ChildReaper::IN_FLIGHT_WAIT),
        (Duration::from_secs(2), Duration::from_secs(2)),
    ] {
        let reaper = ChildReaper::default();
        let _call = reaper.begin_call(None)?;
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
    let (id, mut stuck) =
        ReapableChild::from_child(child(sh.pid, None, &dir.path().join("exit"), &[])?);
    // Like the watcher of a process stuck in the kernel.
    stuck.task = Some(Arc::new(Mutex::new(Some(task::spawn(
        std::future::pending(),
    )))));
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

#[tokio::test(start_paused = true)]
async fn exit_files_wait_for_quiet_output() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let output = attach.output();

    // Nothing flows, a process holds the pipes open.
    let start = Instant::now();
    ReapableChild::wait_for_output(attach.clone(), Instant::now() + DRAIN_MAX_LOG_WAIT).await;
    assert_eq!(start.elapsed(), DRAIN_TIMEOUT);

    // Output which keeps flowing is bounded.
    let writer = task::spawn({
        let attach = attach.clone();
        async move {
            loop {
                attach.write(Message::Data(
                    "data".into(),
                    crate::container_io::Pipe::StdOut,
                ));
                time::sleep(DRAIN_TIMEOUT / 2).await;
            }
        }
    });
    let start = Instant::now();
    ReapableChild::wait_for_output(attach.clone(), Instant::now() + DRAIN_MAX_LOG_WAIT).await;
    assert_eq!(start.elapsed(), DRAIN_MAX_WAIT);
    writer.abort();

    // Ended output does not wait at all.
    drop(output);
    let start = Instant::now();
    ReapableChild::wait_for_output(attach, Instant::now() + DRAIN_MAX_LOG_WAIT).await;
    assert_eq!(start.elapsed(), Duration::ZERO);
    Ok(())
}

/// A read loop over a pipe, whose log drivers hang while a guard from
/// [`hold`] holds the log.
struct LogLoop {
    writer: tokio::io::DuplexStream,
    attach: SharedContainerAttach,
    logger: crate::container_log::SharedContainerLog,
    cri: tempfile::NamedTempFile,
    json: tempfile::NamedTempFile,
    task: JoinHandle<Result<()>>,
}

async fn log_loop() -> Result<LogLoop> {
    let (cri, json, logger) = crate::container_log::test_helpers::container_log(true).await?;
    let attach = SharedContainerAttach::default();
    let (writer, reader) = tokio::io::duplex(1024);
    let task = task::spawn(ContainerIO::read_loop(
        reader,
        crate::container_io::Pipe::StdOut,
        logger.clone(),
        None,
        attach.clone(),
        attach.output(),
    ));
    Ok(LogLoop {
        writer,
        attach,
        logger,
        cri,
        json,
        task,
    })
}

/// Make the log drivers hang until the guard gets dropped.
async fn hold(
    logger: &crate::container_log::SharedContainerLog,
) -> tokio::sync::OwnedRwLockReadGuard<ContainerLog> {
    logger.clone().read_owned().await
}

/// Wait until a read loop has output on its way into the log, failing
/// if it never does.
async fn wait_logging(attach: &SharedContainerAttach) -> Result<()> {
    for _ in 0..1000 {
        if attach.is_logging() {
            return Ok(());
        }
        task::yield_now().await;
    }
    bail!("no output on its way into the log")
}

#[tokio::test(start_paused = true)]
async fn exit_files_wait_for_slow_log_drivers() -> Result<()> {
    let mut l = log_loop().await?;
    let held = hold(&l.logger).await;
    l.writer.write_all(b"last line\n").await?;
    wait_logging(&l.attach).await?;

    // The log takes longer than the drain waits for a pause.
    let wait = task::spawn(ReapableChild::wait_for_output(
        l.attach.clone(),
        Instant::now() + DRAIN_MAX_LOG_WAIT,
    ));
    time::sleep(2 * DRAIN_MAX_WAIT).await;
    assert!(!wait.is_finished());

    drop(held);
    drop(l.writer);
    wait.await?;
    assert!(crate::container_log::test_helpers::read(&l.cri)?.contains("last line"));
    l.task.await?
}

#[tokio::test(start_paused = true)]
async fn exit_files_wait_for_the_last_line() -> Result<()> {
    let mut l = log_loop().await?;
    let (mut rx, _) = l.attach.subscribe();
    l.writer.write_all(b"no newline").await?;
    // Broadcast once the chunk got logged, the line still waits.
    rx.recv().await?;

    // The output ended, its last line is on its way into the log.
    let held = hold(&l.logger).await;
    drop(l.writer);
    wait_logging(&l.attach).await?;
    let wait = task::spawn(ReapableChild::wait_for_output(
        l.attach.clone(),
        Instant::now() + DRAIN_MAX_LOG_WAIT,
    ));
    time::sleep(2 * DRAIN_MAX_WAIT).await;
    assert!(!wait.is_finished());

    drop(held);
    wait.await?;
    assert!(
        crate::container_log::test_helpers::read(&l.json)?
            .contains(r#""message":"no newline","partial":true"#)
    );
    l.task.await?
}

#[tokio::test(start_paused = true)]
async fn nothing_gets_logged_after_the_output_finished() -> Result<()> {
    let mut l = log_loop().await?;
    let (mut rx, _) = l.attach.subscribe();
    l.writer.write_all(b"no newline").await?;
    rx.recv().await?;

    // A process left behind keeps the pipe open, the line waits for its
    // ending. It gets written once the read loop ended.
    ReapableChild::finish_output(l.attach.clone()).await;
    (&mut l.task).await??;
    let read = || crate::container_log::test_helpers::read(&l.json);
    assert!(read()?.contains(r#""message":"no newline","partial":true"#));

    // Its later output does not reach the log anymore.
    assert!(l.writer.write_all(b"late\n").await.is_err());
    assert!(!read()?.contains("late"));
    Ok(())
}

/// The output of an exec whose consumer is slower than the drain after
/// the exit waits for a pause, which blocks the read loop, arrives in
/// full.
#[tokio::test(start_paused = true)]
async fn slow_exec_consumers_get_all_output() -> Result<()> {
    use crate::container_io::{STDIO_CHANNEL_SIZE, StdioSender};

    const OUTPUT: usize = 1024 * 1024;
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
    let attach = io.attach().await;
    let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
    let sender = StdioSender::new(tx, &Arc::new(AtomicBool::new(true)));
    let (mut writer, reader) = tokio::io::duplex(64 * 1024);
    task::spawn(ContainerIO::read_loop(
        reader,
        crate::container_io::Pipe::StdOut,
        ContainerLog::new(),
        Some(sender),
        attach.clone(),
        attach.output(),
    ));
    let write = task::spawn(async move {
        writer.write_all(&vec![b'x'; OUTPUT]).await?;
        Ok::<_, std::io::Error>(writer)
    });

    // The process wrote its output and exited.
    let sh = spawn_sh("exit 0")?;
    let call = reaper.begin_call(None)?;
    let mut child = child(sh.pid, None, &dir.path().join("exit"), &[])?;
    child.kind = ChildKind::Exec;
    child.exit_paths = vec![];
    child.io = io.clone();
    let token = child.token.clone();
    let _stop = attach.stop_output_on_exit(token.clone());
    let mut exit_rx = reaper.watch_grandchild(child, vec![], call)?;
    exit_rx.recv().await?;

    let mut received = 0;
    while received < OUTPUT {
        time::sleep(4 * DRAIN_TIMEOUT).await;
        match rx.recv().await? {
            Message::Data(data, _) => received += data.len(),
            Message::Done => break,
        }
    }
    assert_eq!(received, OUTPUT);
    drop(write.await??);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn finished_output_is_bounded_with_a_hanging_log() -> Result<()> {
    let mut l = log_loop().await?;
    let held = hold(&l.logger).await;
    l.writer.write_all(b"never logged\n").await?;
    wait_logging(&l.attach).await?;

    let start = Instant::now();
    ReapableChild::finish_output(l.attach.clone()).await;
    assert_eq!(start.elapsed(), DRAIN_MAX_LOG_WAIT);

    drop(held);
    drop(l.writer);
    l.task.await?
}

#[tokio::test(start_paused = true)]
async fn exit_files_wait_for_log_drivers_bounded() -> Result<()> {
    let mut l = log_loop().await?;
    let held = hold(&l.logger).await;
    l.writer.write_all(b"never logged\n").await?;
    wait_logging(&l.attach).await?;

    let start = Instant::now();
    ReapableChild::wait_for_output(l.attach, Instant::now() + DRAIN_MAX_LOG_WAIT).await;
    assert_eq!(start.elapsed(), DRAIN_MAX_LOG_WAIT);

    drop(held);
    drop(l.writer);
    l.task.await?
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
            ..RuntimeCall::new(Path::new("sh"), &args, &pidfile)
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
