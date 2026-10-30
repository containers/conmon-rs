use super::*;
use crate::test_util::Guarded;
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
