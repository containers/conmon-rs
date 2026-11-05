use super::*;
use crate::{
    child_reaper::ChildReaper,
    test_util::{Guarded, wait_for},
};
use nix::sys::wait::WaitPidFlag;
use std::{process::Command as StdCommand, sync::Arc};

#[tokio::test]
async fn hanging_setup_gets_killed() -> Result<()> {
    let mut child = Command::new("sleep")
        .arg("600")
        .kill_on_drop(true)
        .spawn()?;
    // Not with paused time, which would not wait for the kill either.
    let status = Pause::wait_or_kill(&mut child, Duration::from_millis(100))
        .await
        .context("pause child did not exit")??;
    assert_eq!(status.signal(), Some(Signal::SIGKILL as i32));
    Ok(())
}

#[test]
fn failed_setups_get_undone_unless_the_pause_process_did() {
    assert!(Pause::check_setup(Ok(ExitStatus::from_raw(0))).is_ok());
    // It exited on its own and undid its setup.
    assert!(matches!(
        Pause::check_setup(Ok(ExitStatus::from_raw(1 << 8))),
        Err(SetupFailure::Undone(_))
    ));
    // It got killed, or nobody knows what it did.
    assert!(matches!(
        Pause::check_setup(Ok(ExitStatus::from_raw(Signal::SIGKILL as i32))),
        Err(SetupFailure::Left(_))
    ));
    let err = Pause::check_setup(Err(std::io::Error::from_raw_os_error(libc::ECHILD)));
    let Err(SetupFailure::Left(e)) = err else {
        panic!("wait error not undone: {err:?}");
    };
    assert!(
        format!("{e:#}").starts_with("wait for pause child: "),
        "{e:#}"
    );
}

#[test]
fn dead_pause_process_is_noticed() -> Result<()> {
    let pause = |process| Pause {
        base_path: "/var/run".into(),
        pod_id: "pod".into(),
        namespaces: vec![Namespace::Pid],
        uid_mappings: vec![],
        gid_mappings: vec![],
        process,
        exited: Arc::default(),
    };
    assert!(pause(None).is_alive());

    // An init which the orphan sweep reaped before it was opened.
    let reaped = pause(None);
    reaped.exited.store(true, Ordering::SeqCst);
    assert!(!reaped.is_alive());

    let mut child = Guarded::spawn(StdCommand::new("sleep").arg("60"))?;
    let running = pause(Some(Process::open(child_pid(child.0.id())?)));
    assert!(running.is_alive());
    running.exited.store(true, Ordering::SeqCst);
    assert!(!running.is_alive());

    // Also before the reaper ran, as a zombie and once reaped.
    let running = pause(Some(Process::open(child_pid(child.0.id())?)));
    child.0.kill()?;
    let pid = nix::sys::wait::Id::Pid(child_pid(child.0.id())?);
    nix::sys::wait::waitid(pid, WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT)?;
    assert!(!running.is_alive());
    child.0.wait()?;
    assert!(!running.is_alive());
    Ok(())
}

/// The init which a killed pause subcommand left behind as a child of the
/// server gets stopped along with the failed setup.
#[test]
fn left_init_gets_stopped() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (base_path, pod_id) = (dir.path(), "pod");
    let pid_path = Pause::pause_pid_path(base_path, pod_id);
    let mut child = Guarded::spawn(StdCommand::new("sleep").arg("60"))?;
    let pid = child_pid(child.0.id())?;

    // Not without the PID file, nor for a PID which is no child.
    assert!(Pause::left_init(base_path, pod_id, &[Namespace::Pid]).is_none());
    fs::create_dir_all(pid_path.parent().context("no parent")?)?;
    fs::write(&pid_path, "1")?;
    assert!(Pause::left_init(base_path, pod_id, &[Namespace::Pid]).is_none());

    // Nor without a PID namespace, which has no init.
    fs::write(&pid_path, pid.to_string())?;
    assert!(Pause::left_init(base_path, pod_id, &[Namespace::Net]).is_none());
    let process = Pause::left_init(base_path, pod_id, &[Namespace::Pid]).context("no init")?;
    assert_eq!(process.pid(), pid);

    let pause = Pause {
        base_path: base_path.into(),
        pod_id: pod_id.into(),
        namespaces: vec![],
        uid_mappings: vec![],
        gid_mappings: vec![],
        process: Some(process),
        exited: Arc::default(),
    };
    pause.stop();
    assert!(!pid_path.exists());
    assert_eq!(child.0.wait()?.signal(), Some(Signal::SIGTERM as i32));
    Ok(())
}

/// Undoing a setup whose subcommand got killed stops the init it left before
/// removing its PID file, and the error carries what the subcommand printed
/// so far, without waiting for a writer which still holds the pipe.
#[tokio::test]
async fn undone_left_setups_stop_the_init_and_keep_stderr() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (base_path, pod_id) = (dir.path(), "pod");
    let pid_path = Pause::pause_pid_path(base_path, pod_id);
    let mut init = Guarded::spawn(StdCommand::new("sleep").arg("60"))?;
    fs::create_dir_all(pid_path.parent().context("no parent")?)?;
    fs::write(&pid_path, child_pid(init.0.id())?.to_string())?;

    // A subcommand which printed the reason and whose child holds stderr.
    let mut subcommand = Command::new("sh")
        .args(["-c", "echo oops >&2; exec sleep 60"])
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let output = RuntimeOutput::spawn(None, subcommand.stderr.take());
    wait_for(|| output.has_stderr()).await?;

    let pause = Pause {
        base_path: base_path.into(),
        pod_id: pod_id.into(),
        namespaces: vec![Namespace::Pid],
        uid_mappings: vec![],
        gid_mappings: vec![],
        process: None,
        exited: Arc::default(),
    };
    let start = std::time::Instant::now();
    let e = pause
        .undo(output, anyhow::format_err!("killed"), true)
        .await;
    // Without waiting for the end of stderr, which the sleep holds open for
    // longer than the grace a finish of the output would give it.
    assert!(
        start.elapsed() < ChildReaper::RUNTIME_GRACE - Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(format!("{e:#}"), "killed: oops");
    assert!(!pid_path.exists());
    assert_eq!(init.0.wait()?.signal(), Some(Signal::SIGTERM as i32));
    Ok(())
}

#[test]
fn ensure_matches_config() {
    let mappings = vec!["0 0 1".to_string()];
    let pause = Pause {
        base_path: "/var/run".into(),
        pod_id: "pod".into(),
        namespaces: vec![Namespace::Ipc, Namespace::User],
        uid_mappings: mappings.clone(),
        gid_mappings: mappings.clone(),
        process: None,
        exited: Arc::default(),
    };
    let namespaces = [Namespace::User, Namespace::Ipc];
    let base_path = Path::new("/var/run/");

    assert!(
        pause
            .ensure_matches(base_path, "pod", &namespaces, &mappings, &mappings)
            .is_ok()
    );
    assert!(
        pause
            .ensure_matches(base_path, "other", &namespaces, &mappings, &mappings)
            .is_err()
    );
    assert!(
        pause
            .ensure_matches(Path::new("/tmp"), "pod", &namespaces, &mappings, &mappings)
            .is_err()
    );
    assert!(
        pause
            .ensure_matches(base_path, "pod", &[Namespace::Ipc], &mappings, &mappings)
            .is_err()
    );
    assert!(
        pause
            .ensure_matches(base_path, "pod", &namespaces, &[], &mappings)
            .is_err()
    );
}

#[test]
fn location_stays_below_the_base_path() {
    let base_path = Path::new("/var/run");
    for pod_id in [
        "pod",
        "3f5c0d2e-6a1b-4c8e-9f7a-1b2c3d4e5f60",
        "a.v1",
        "-pod",
        "..pod",
    ] {
        assert!(
            Pause::validate_location(base_path, pod_id).is_ok(),
            "{pod_id}"
        );
    }
    for pod_id in [
        "",
        ".",
        "..",
        "/etc/passwd",
        "../pod",
        "a/b",
        "pod\n",
        "pöd",
    ] {
        assert!(
            Pause::validate_location(base_path, pod_id).is_err(),
            "{pod_id}"
        );
    }
    assert!(Pause::validate_location(Path::new(""), "pod").is_err());
    assert!(Pause::validate_location(Path::new("run"), "pod").is_err());
    // The PID file name has to fit into NAME_MAX.
    assert!(Pause::validate_location(base_path, &"a".repeat(251)).is_ok());
    assert!(Pause::validate_location(base_path, &"a".repeat(252)).is_err());
}

#[test]
fn pause_pid_paths_of_pods_differ() {
    assert_eq!(
        Pause::pause_pid_path("/var/run", "a.v1"),
        Path::new("/var/run/conmonrs/a.v1.pid")
    );
    assert_ne!(
        Pause::pause_pid_path("/var/run", "a.v1"),
        Pause::pause_pid_path("/var/run", "a.v2")
    );
    assert_eq!(
        Namespace::Net.path("/var/run", "a.v1"),
        Path::new("/var/run/netns/a.v1")
    );
}

/// Needs root to bind a namespace and passes without, so it only runs
/// locally, like with `unshare -rm cargo test -p conmonrs
/// busy_namespace_bind_gets_detached`. The unit tests of CI run without root.
#[test]
fn busy_namespace_bind_gets_detached() -> Result<()> {
    if !nix::unistd::geteuid().is_root() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let (namespace, pod) = (Namespace::Uts, "pod");
    let path = namespace.path(dir.path(), pod);
    namespace.bind(dir.path(), pod, nix::unistd::getpid())?;
    // A plain unmount fails while the bind is open.
    let busy = File::open(&path)?;
    namespace.umount(dir.path(), pod)?;
    assert!(!path.exists());
    drop(busy);

    // A bind path which never got mounted, like after a failed setup.
    File::create(&path)?;
    namespace.umount(dir.path(), pod)?;
    assert!(!path.exists());
    Ok(())
}
