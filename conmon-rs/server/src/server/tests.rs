use super::*;
use std::os::fd::AsRawFd;

#[test]
fn inherited_fds_reports_descriptors_above_stderr() -> Result<()> {
    let file = tempfile::tempfile()?;
    let fd = file.as_raw_fd();

    let fds = Server::inherited_fds();

    assert!(
        fds.contains(&fd),
        "expected an open descriptor {fd} to be reported, got {fds:?}"
    );
    // stdin, stdout and stderr are never reported, they are not ours to close.
    assert!(!fds.contains(&0));
    assert!(!fds.contains(&1));
    assert!(!fds.contains(&2));

    Ok(())
}

#[tokio::test]
async fn cleanup_removes_the_socket() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket = dir.path().join("conmon.sock");
    File::create(&socket)?;

    let reaper = Arc::new(ChildReaper::default());
    let deadline = || Instant::now() + Duration::from_secs(10);
    Server::cleanup(reaper.clone(), &socket, deadline()).await?;
    assert!(!socket.exists());

    // A socket which got removed already, like with its directory, is fine.
    Server::cleanup(reaper.clone(), &socket, deadline()).await?;

    // Other failures are reported.
    std::fs::create_dir(&socket)?;
    let err = Server::cleanup(reaper, &socket, deadline())
        .await
        .err()
        .map(|e| format!("{e:#}"));
    assert!(err.is_some_and(|e| e.starts_with("remove socket file: ")));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_parts_report_what_did_not_finish() {
    let deadline = Instant::now() + Duration::from_secs(1);
    assert!(Server::until(deadline, async { Ok(()) }).await.is_ok());
    let hung = Server::until(deadline, std::future::pending()).await;
    assert_eq!(Instant::now(), deadline);

    let err = Server::all_ok([
        Ok(()),
        hung.context("remove socket file"),
        Err(format_err!("PIDs [1] did not exit")).context("kill grandchildren"),
    ])
    .err()
    .map(|e| format!("{e:#}"));
    assert_eq!(
        err.as_deref(),
        Some(
            "remove socket file: did not finish in time, \
             kill grandchildren: PIDs [1] did not exit"
        )
    );
    assert!(Server::all_ok([Ok(()), Ok(())]).is_ok());
}

/// Collects the log of a test.
#[derive(Clone, Default)]
struct Log(Arc<std::sync::Mutex<Vec<u8>>>);

impl Log {
    fn contents(&self) -> Result<String> {
        let log = self.0.lock().map_err(|_| format_err!("poisoned"))?.clone();
        Ok(String::from_utf8(log)?)
    }
}

impl Write for Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("poisoned"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn unfinished_shutdown_is_logged_and_ends_the_server_with_it() -> Result<()> {
    let log = Log::default();
    let writer = log.clone();
    let _subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .set_default();
    let dir = tempfile::tempdir()?;
    // A socket which cannot be removed.
    let socket = dir.path().join("conmon.sock");
    std::fs::create_dir(&socket)?;

    let result = Server::shutdown(
        Arc::new(ChildReaper::default()),
        &socket,
        Instant::now() + Server::CLEANUP_TIMEOUT,
    )
    .await;
    let err = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
    assert!(err.starts_with("remove socket file: "), "{err}");
    let logged = log.contents()?;
    assert!(
        logged.contains("ERROR") && logged.contains("Shutdown did not finish: remove socket file"),
        "{logged}"
    );

    // The backend ends the server with it, which then exits non-zero. It
    // got logged already.
    let (tx, rx) = oneshot::channel();
    tx.send(Err(format_err!("remove socket file: failed")))
        .map_err(|_| format_err!("send"))?;
    assert!(Server::logged(Server::shutdown_result(rx.await)).is_err());
    let (tx, rx) = oneshot::channel();
    tx.send(Ok(())).map_err(|_| format_err!("send"))?;
    Server::logged(Server::shutdown_result(rx.await))?;
    assert_eq!(log.contents()?.matches("remove socket file").count(), 1);

    // Without a result, the server failed itself, which gets logged.
    let (tx, rx) = oneshot::channel::<Result<()>>();
    drop(tx);
    assert!(Server::logged(Server::shutdown_result(rx.await)).is_err());
    let log = log.contents()?;
    assert!(
        log.contains("ERROR") && log.contains("signal handler ended without a result"),
        "{log}"
    );
    Ok(())
}

/// The server logs a backend which fails, like one which cannot bind its
/// socket. With the journal as log driver, the standard streams are gone
/// after the fork.
#[tokio::test]
async fn backend_failures_get_logged() -> Result<()> {
    let log = Log::default();
    let writer = log.clone();
    let _subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .set_default();
    let dir = tempfile::tempdir()?;
    let config = Config::parse_for_test([
        "--runtime",
        "/bin/true",
        "--runtime-dir",
        dir.path().to_str().context("runtime dir")?,
    ]);
    // Taken, like by another server.
    std::fs::create_dir(config.socket())?;
    let (_shutdown_tx, shutdown_rx) = oneshot::channel();
    let result = time::timeout(
        Duration::from_secs(10),
        Server::with_config(config).start_backend(shutdown_rx),
    )
    .await?;
    let err = Server::logged(result).err().context("bind succeeded")?;
    assert!(format!("{err:#}").starts_with("bind socket "), "{err:#}");
    let log = log.contents()?;
    assert!(log.contains(&format!("ERROR {err:#}")), "{log}");
    Ok(())
}
