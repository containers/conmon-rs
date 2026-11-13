use super::*;
use crate::{
    child_reaper::{Child, InFlight},
    test_util::{Log, container_child, run_in_own_process, spawn_sh, wait_for_real},
};
use nix::sys::{prctl, signal::raise};

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

#[tokio::test(start_paused = true)]
async fn unfinished_shutdown_is_logged_and_ends_the_server_with_it() -> Result<()> {
    let (log, _subscriber) = Log::capture();
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

/// The signal handler sends its failures and those of the shutdown to the
/// backend, also when it cannot listen for signals.
#[tokio::test(start_paused = true)]
async fn signal_handler_sends_the_failures_to_the_backend() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // A socket which cannot be removed.
    let socket = dir.path().join("conmon.sock");
    std::fs::create_dir(&socket)?;

    let (tx, rx) = oneshot::channel();
    Server::start_signal_handler(
        Err(format_err!("no signals")),
        Arc::new(ChildReaper::default()),
        &socket,
        tx,
    )
    .await;
    let err = rx
        .await?
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(err.starts_with("no signals, remove socket file: "), "{err}");
    Ok(())
}

/// The server logs a backend which fails, like one which cannot bind its
/// socket. With the journal as log driver, the standard streams are gone
/// after the fork.
#[tokio::test]
async fn backend_failures_get_logged() -> Result<()> {
    let (log, _subscriber) = Log::capture();
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

/// Set for the test process which runs the signal test.
const SIGNAL_TEST_ENV: &str = "CONMONRS_SIGNAL_TEST";

/// SIGTERM, SIGINT and SIGQUIT shut the server down. The test raises them
/// for its own process, which one the server does not listen for would end,
/// so it runs in a process of its own.
#[test]
fn signals_shut_the_server_down() -> Result<()> {
    let test = "server::tests::signals_in_own_process";
    run_in_own_process(test, SIGNAL_TEST_ENV, false)
}

#[tokio::test]
#[ignore = "run by signals_shut_the_server_down in a process of its own"]
async fn signals_in_own_process() -> Result<()> {
    if std::env::var_os(SIGNAL_TEST_ENV).is_none() {
        return Ok(());
    }
    // A signal which ends the process must not leave a core dump, also
    // not through a core pattern with a pipe, which no limit stops.
    prctl::set_dumpable(false)?;
    let (log, _subscriber) = Log::capture();
    let wait = Duration::from_secs(60);

    let reaper = ChildReaper::default();
    for (signal, name) in [
        (Signal::SIGTERM, "SIGTERM"),
        (Signal::SIGINT, "SIGINT"),
        (Signal::SIGQUIT, "SIGQUIT"),
    ] {
        let signals = Server::listen_for_signals();
        let before = Instant::now();
        raise(signal)?;
        let deadline = time::timeout(wait, Server::wait_for_signal(signals, &reaper)).await??;
        assert!(deadline >= before + Server::CLEANUP_TIMEOUT, "{name}");
        let log = log.contents()?;
        assert!(log.contains(&format!("Received {name}")), "{name}: {log}");
    }

    // A container which ignores SIGTERM takes its whole grace, unless
    // another signal asks for the shutdown meanwhile.
    let sh = spawn_sh("trap '' TERM; exec sleep 60")?;
    let comm = format!("/proc/{}/comm", sh.pid);
    wait_for_real(|| std::fs::read_to_string(&comm).is_ok_and(|c| c == "sleep\n")).await;
    let container = Child {
        pid: sh.pid,
        ..container_child(false)?
    };
    reaper.watch_grandchild(container, InFlight::default())?;
    for (signal, name) in [(Signal::SIGINT, "SIGINT"), (Signal::SIGQUIT, "SIGQUIT")] {
        // Logged right before the containers get their grace.
        let terminating = "Received SIGTERM";
        let count = log.contents()?.matches(terminating).count();
        let signals = Server::listen_for_signals();
        raise(Signal::SIGTERM)?;
        let again = async {
            wait_for_real(|| {
                log.contents()
                    .is_ok_and(|log| log.matches(terminating).count() > count)
            })
            .await;
            raise(signal)
        };
        let (deadline, raised) = time::timeout(wait, async {
            tokio::join!(Server::wait_for_signal(signals, &reaper), again)
        })
        .await?;
        deadline?;
        raised?;
        let log = log.contents()?;
        let expected = format!("Received {name}, not waiting for the containers");
        assert!(log.contains(&expected), "{name}: {log}");
    }
    assert!(sh.process.is_running());
    Ok(())
}

/// A build which cannot trace says so, once at its start, instead of
/// ignoring the flag silently.
#[cfg(not(feature = "tracing"))]
#[test]
fn tracing_without_the_feature_gets_a_warning() -> Result<()> {
    let (log, _subscriber) = Log::capture();
    // Without a log driver, the logging sets up nothing for the process.
    let init = |args: &[&str]| {
        let config = Config::parse_for_test([&["--log-driver", "none"], args].concat());
        Server::with_config(config).init_logging()
    };
    init(&[])?;
    let logged = log.contents()?;
    assert!(!logged.contains("WARN"), "{logged}");

    init(&["--enable-tracing"])?;
    let logged = log.contents()?;
    assert_eq!(logged.matches("WARN").count(), 1, "{logged}");
    assert!(logged.contains("Ignoring --enable-tracing"), "{logged}");
    Ok(())
}
