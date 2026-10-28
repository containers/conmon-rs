use super::*;
use crate::{
    child_reaper::ChildReaper,
    config::Config,
    conmon_capnp::conmon::CgroupManager,
    streaming_server::{ADDR, ExecSession, StreamingServer, testing::*},
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
    os::unix::{fs::PermissionsExt, process::CommandExt, process::ExitStatusExt},
    path::Path,
    sync::Arc,
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    task::LocalSet,
    time::{self, Duration},
};

/// A runtime which takes `delay` to start the process, which then
/// writes its output, and its config, none without the config of a
/// test.
fn slow_runtime(dir: &Path, delay: &str) -> Result<Option<Config>> {
    let runtime = dir.join("runtime");
    std::fs::write(
        &runtime,
        format!(
            "#!/bin/sh\n\
             sleep {delay}\n\
             for arg; do case $arg in --pid-file=*) pidfile=${{arg#--pid-file=}};; esac; done\n\
             sh -c 'echo started' &\n\
             echo $! >\"$pidfile\"\n"
        ),
    )?;
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))?;
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
    let mut server = StreamingServer::default();
    server.start_if_required().await?;
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
    let path = url
        .strip_prefix(&format!("http://{ADDR}:{}", server.port))
        .context("url")?;
    let mut stream = TcpStream::connect((ADDR, server.port)).await?;
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {ADDR}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Protocol: {PROTOCOL_V5}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    let response = response_head(&mut stream).await?;
    assert!(response.starts_with("http/1.1 101"), "{response}");
    Ok((server, stream))
}

#[tokio::test]
async fn slow_exec_starts_are_no_idleness() -> Result<()> {
    // The runtime takes longer than the idle timeout.
    let dir = tempfile::tempdir()?;
    let Some(config) = slow_runtime(dir.path(), "1")? else {
        return Ok(());
    };

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
    let Some(config) = slow_runtime(dir.path(), "4")? else {
        return Ok(());
    };

    LocalSet::new()
        .run_until(async {
            let (_server, stream) = exec_websocket(config, None).await?;
            let (mut reader, writer) = send_too_large(stream);

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
    let mut output = Output::spdy(output);

    let mut process = std::process::Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()?;
    let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
    // Like a watcher which got aborted before it sent the exit.
    let exit_rx = broadcast::channel(1).1;
    let attach = io.attach().await;
    let stopped = attach.output_stopped().clone();
    let token = CancellationToken::new();
    let teardown = ExecTeardown {
        stdio: Some(io.stdio().await?),
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
