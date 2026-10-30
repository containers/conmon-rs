use super::*;
use crate::{
    child_reaper::ChildReaper,
    config::Config,
    conmon_capnp::conmon::CgroupManager,
    container_log::ContainerLog,
    streaming_server::{ExecSession, MAX_MESSAGE_SIZE, StreamingServer, testing::*},
};
use anyhow::{Context, Result};
use bytes::Bytes;

use crate::streaming_server::{remotecommand::STREAM_ERR_BYTE, web};
use crate::{
    process::child_pid,
    streaming_server::{
        remotecommand::{ExpectedStreams, SpdyStreams, Version},
        session::ClientRx,
        spdy::{Connection, testing::Client},
    },
};
use std::{
    os::unix::{process::CommandExt, process::ExitStatusExt},
    path::Path,
    sync::Arc,
};
use tokio::{
    net::TcpStream,
    task::LocalSet,
    time::{self, Duration},
};

/// A runtime which takes `delay` to start the process, which then
/// writes its output, and its config.
fn slow_runtime(dir: &Path, delay: &str) -> Result<Config> {
    let runtime = dir.join("runtime");
    crate::test_util::write_script(
        &runtime,
        &format!(
            "sleep {delay}\n\
             for arg; do case $arg in --pid-file=*) pidfile=${{arg#--pid-file=}};; esac; done\n\
             sh -c 'echo started' &\n\
             echo $! >\"$pidfile\""
        ),
    )?;
    Ok(Config::parse_for_test([
        "--runtime",
        runtime.to_str().context("runtime path")?,
        "--runtime-dir",
        dir.to_str().context("runtime dir")?,
    ]))
}

/// An exec session over a WebSocket with the runtime of the config.
async fn exec_websocket(
    config: Config,
    idle_timeout: Option<Duration>,
) -> Result<(StreamingServer, TcpStream)> {
    let server = StreamingServer::default();
    let url = server
        .exec_url(ExecSession {
            child_reaper: Arc::new(ChildReaper::default()),
            tty: false,
            server_config: Arc::new(config),
            cgroup_manager: CgroupManager::Cgroupfs,
            container_id: "id".into(),
            command: vec!["true".into()],
            stdin: false,
            stdout: true,
            stderr: false,
            idle_timeout,
        })
        .await?;
    let (response, stream) = upgrade(&server, &url, "GET", &websocket_headers(PROTOCOL_V5)).await?;
    assert!(response.starts_with("http/1.1 101"), "{response}");
    Ok((server, stream))
}

#[tokio::test]
async fn slow_exec_starts_are_no_idleness() -> Result<()> {
    // The runtime takes longer than the idle timeout.
    let dir = tempfile::tempdir()?;
    let config = slow_runtime(dir.path(), "1")?;

    LocalSet::new()
        .run_until(async {
            let (_server, mut stream) =
                exec_websocket(config, Some(Duration::from_millis(500))).await?;

            // The output arrives, the wait for the runtime did not end
            // the session.
            let mut output = vec![];
            loop {
                match server_frame(&mut stream).await? {
                    (0x8, _) => break,
                    (_, payload) => output.extend(payload),
                }
            }
            let output = String::from_utf8_lossy(&output);
            assert!(output.contains("started"), "{output}");
            assert!(!output.contains("idle timeout"), "{output}");
            answer_close(&mut stream).await
        })
        .await
}

#[tokio::test]
async fn too_large_messages_fail_slow_exec_starts() -> Result<()> {
    // The runtime takes longer than the session waits for its end.
    let dir = tempfile::tempdir()?;
    let config = slow_runtime(dir.path(), "4")?;

    LocalSet::new()
        .run_until(async {
            let (_server, stream) = exec_websocket(config, None).await?;
            let (mut reader, writer) = send_too_large(stream, MAX_MESSAGE_SIZE);

            // The status and the close come without the runtime.
            let (opcode, status) =
                time::timeout(web::CLOSE_TIMEOUT / 2, server_frame(&mut reader)).await??;
            assert_eq!((opcode, status.first()), (2, Some(&STREAM_ERR_BYTE)));
            let status = String::from_utf8_lossy(&status[1..]);
            assert!(status.contains(MESSAGE_TOO_LARGE), "{status}");
            too_large_close(reader, writer).await
        })
        .await
}

#[tokio::test]
async fn lost_exit_ends_the_session_and_kills_the_process() -> Result<()> {
    // The output goes to a SPDY client.
    let (client, server) = tokio::io::duplex(64 * 1024);
    let mut connection =
        Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
    let mut client = Client::new(client)?;
    client.open(1, [("streamType", "error")].into()).await?;
    client.open(3, [("streamType", "stdout")].into()).await?;
    let expected = ExpectedStreams {
        stdin: false,
        stdout: true,
        stderr: false,
    };
    let streams = SpdyStreams::wait(&mut connection, expected).await?;
    let (_input, output) = streams.into_parts(connection, Version::V4).await;
    let mut output = Output::spdy(output, None);

    let mut process = std::process::Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()?;
    let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
    // Like a watcher which got aborted before it sent the exit.
    let exit_rx = broadcast::channel(1).1;
    let attach = io.attach().clone();
    let stopped = attach.output_stopped().clone();
    let token = CancellationToken::new();
    let teardown = ExecTeardown {
        stdio: io.stdio()?,
        _stop_output: attach.stop_output_on_exit(token.clone()),
        attach,
        token,
        process: Process::open(child_pid(process.id())?),
        kill: Arc::default(),
    };
    let started = StartedExec {
        io,
        early_stdio: EarlyStdio::default(),
        exit_rx,
        stdin_enabled: false,
        stdout_enabled: true,
        stderr_enabled: false,
        teardown,
    };
    let (client_rx, _senders) = ClientRx::idle();
    let mut status = None;
    time::timeout(
        Duration::from_secs(5),
        StreamingServer::exec_loop(started, &mut output, client_rx, &mut status),
    )
    .await??;
    assert!(
        matches!(&status, Some(Status::Error(e)) if e.contains("unable to get the exit")),
        "{status:?}"
    );
    let exit = tokio::task::spawn_blocking(move || process.wait()).await??;
    assert_eq!(exit.signal(), Some(Signal::SIGKILL as i32));
    // Nothing waits for the read loops, which a lost exit never stops.
    assert!(stopped.is_cancelled());
    Ok(())
}

/// Drop the teardown of a running process without the kill, like after the
/// client went away, and return the process and the attach of its IO.
fn tear_down_without_kill(tty: bool) -> Result<(std::process::Child, SharedContainerAttach)> {
    let process = std::process::Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()?;
    let io = SharedContainerIO::new(ContainerIO::new(tty, ContainerLog::new())?);
    let attach = io.attach().clone();
    let token = CancellationToken::new();
    drop(ExecTeardown {
        stdio: io.stdio()?,
        _stop_output: attach.stop_output_on_exit(token.clone()),
        attach: attach.clone(),
        token,
        process: Process::open(child_pid(process.id())?),
        kill: Arc::default(),
    });
    Ok((process, attach))
}

#[tokio::test]
async fn client_gone_closes_stdin_and_stops_the_output() -> Result<()> {
    let (mut process, attach) = tear_down_without_kill(false)?;
    assert!(attach.stdin_closed().is_cancelled());
    // The writes of the process fail, it does not run on with its output
    // discarded.
    assert!(attach.output_stopped().is_cancelled());
    process.kill()?;
    process.wait()?;
    Ok(())
}

#[tokio::test]
async fn client_gone_hangs_up_the_terminal_and_keeps_reading() -> Result<()> {
    let (mut process, attach) = tear_down_without_kill(true)?;
    assert!(!attach.stdin_closed().is_cancelled());
    // A process which ignores the hang-up must not block on a full
    // terminal, its output gets read until the exit.
    assert!(!attach.output_stopped().is_cancelled());
    let exit = tokio::task::spawn_blocking(move || process.wait()).await??;
    assert_eq!(exit.signal(), Some(Signal::SIGHUP as i32));
    Ok(())
}

#[test]
fn truncated_early_output_fails_the_session() {
    let exit = || {
        Ok(ExitChannelData {
            exit_code: 0,
            timed_out: false,
        })
    };
    assert_eq!(
        StreamingServer::exit_status(exit(), false),
        Status::exited(0)
    );
    // The client must not take output with a gap for a success.
    let status = StreamingServer::exit_status(exit(), true);
    assert!(
        matches!(&status, Status::Error(e) if e.contains("more than 16 MiB") && e.contains("exit code 0")),
        "{status:?}"
    );
}

#[test]
#[expect(clippy::cast_possible_truncation, reason = "a wrapping byte pattern")]
fn early_output_gets_split_into_frames() {
    let data: Vec<u8> = (0..2 * EARLY_FRAME_SIZE + 10).map(|i| i as u8).collect();
    let frames = StreamingServer::early_frames(data.clone());
    assert_eq!(
        frames.iter().map(Bytes::len).collect::<Vec<_>>(),
        [EARLY_FRAME_SIZE, EARLY_FRAME_SIZE, 10]
    );
    assert_eq!(frames.iter().flatten().copied().collect::<Vec<_>>(), data);
    assert!(StreamingServer::early_frames(vec![]).is_empty());
}

#[tokio::test]
async fn terminal_gets_sizes_while_the_runtime_runs() -> Result<()> {
    let terminal = crate::terminal::Terminal::new()?;
    let resizer = terminal.resizer();
    let (resize_tx, resize_rx) = tokio::sync::watch::channel(None);
    // The runtime and the resizing share the task, a yield of the runtime
    // lets the resizing take what changed.
    let settle = || async {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    };
    let run = async {
        // Clients send a size without rows or columns before they know
        // theirs.
        resize_tx.send_replace(Some(Bytes::from_static(br#"{"Width":0,"Height":0}"#)));
        settle().await;
        assert_eq!(resizer.pending(), None);
        resize_tx.send_replace(Some(Bytes::from_static(br#"{"Width":80,"Height":24}"#)));
        settle().await;
        assert_eq!(resizer.pending(), Some((80, 24)));
        // A client which went away ends the resizing, not the runtime.
        drop(resize_tx);
        settle().await;
        anyhow::Ok(1)
    };
    let output = StreamingServer::resizing(run, resize_rx, Some(terminal.resizer())).await?;
    assert_eq!(output, 1);
    Ok(())
}
