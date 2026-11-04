//! Watching a child until it exited and reporting its exit.

use super::*;

#[tokio::test(start_paused = true)]
async fn watch_reports_exit_codes() -> Result<()> {
    check_watch(&[("exit 3", 3), ("kill -9 $$", 137), ("exit 0", 0)]).await
}

/// Like conmon, also for the real-time signals nix does not know. On its own,
/// qemu-user reports them with other numbers, see the test-unit-arch jobs.
#[tokio::test(start_paused = true)]
async fn watch_reports_real_time_signal_exits() -> Result<()> {
    check_watch(&[("kill -34 $$", 162)]).await
}

async fn check_watch(cases: &[(&str, i32)]) -> Result<()> {
    let dir = tempfile::tempdir()?;
    for &(script, expected) in cases {
        let exit_path = dir.path().join("exit");
        let sh = spawn_sh(script)?;
        let (_, reapable, exit) = ReapableChild::from_child(child(sh.pid, None, &exit_path, &[])?);
        let mut exit_rx = reapable.watch(exit, || {});

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

#[tokio::test(start_paused = true)]
async fn watch_kills_on_timeout() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let exit_path = dir.path().join("exit");
    let cleaned = dir.path().join("cleaned");
    let cleanup = ["touch", cleaned.to_str().context("path")?];

    let sh = spawn_sh("exec sleep 60")?;
    let pid = sh.pid;
    let timeout = Instant::now() + Duration::from_secs(1);
    let (_, reapable, exit) =
        ReapableChild::from_child(child(pid, Some(timeout), &exit_path, &cleanup)?);
    let mut exit_rx = reapable.watch(exit, || {});

    let exit = exit_rx.recv().await?;
    assert_eq!(exit.exit_code, EXIT_CODE_TIMED_OUT);
    assert!(exit.timed_out);
    assert_eq!(std::fs::read_to_string(&exit_path)?, "-3");
    wait_for_real(|| is_gone(pid)).await;
    wait_for_real(|| cleaned.exists()).await;
    Ok(())
}
