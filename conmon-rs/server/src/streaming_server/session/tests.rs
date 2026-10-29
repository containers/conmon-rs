use super::*;
use crate::{
    container_io::{ContainerIO, Message as IOMessage, Pipe, SharedContainerIO},
    container_log::ContainerLog,
    streaming_server::{
        AttachSession, StreamingServer,
        remotecommand::{
            ClientEvent, MESSAGE_TOO_LARGE, STDIN_BYTE, STREAM_ERR_BYTE, Status, Stdin,
        },
        session::PENDING_STDIN_MAX_BYTES,
        spdy::testing::{Client, Frame},
        testing::*,
        web::CloseCode,
    },
    test_util::wait_for,
};
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;

use nix::sys::socket::{setsockopt, sockopt};
use std::ops::ControlFlow;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::watch,
    task::LocalSet,
    time::{self, Duration},
};

#[tokio::test]
async fn stdin_before_close_reaches_the_container() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut stream) = attach_session(true, false).await?;

            // More stdin than the channels on the way hold, right before
            // the close, in one write.
            let mut frames = vec![];
            for i in 0..50u8 {
                frames.extend(client_frame(0x2, &[STDIN_BYTE, i]));
            }
            frames.extend(client_frame(0x8, &1000u16.to_be_bytes()));
            stream.write_all(&frames).await?;

            // The container writes output meanwhile, which cannot be sent
            // after the close anymore. The stdin which waits gets merged.
            let attach = io.attach().await;
            let mut stdin = vec![];
            while stdin.len() < 50 {
                attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                stdin.extend_from_slice(&data);
            }
            assert_eq!(stdin, (0..50u8).collect::<Vec<_>>());
            Ok(())
        })
        .await
}

#[tokio::test]
async fn messages_larger_than_the_limit_fail_the_session() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (_io, _server, stream) = attach_session(true, false).await?;
            let (mut reader, writer) = send_too_large(stream);

            // The client learns why, from the status and the close.
            let (opcode, status) = server_frame(&mut reader).await?;
            assert_eq!((opcode, status.first()), (2, Some(&STREAM_ERR_BYTE)));
            let status = String::from_utf8_lossy(&status[1..]);
            assert!(status.contains("\"Failure\""), "{status}");
            assert!(status.contains(MESSAGE_TOO_LARGE), "{status}");
            too_large_close(reader, writer).await
        })
        .await
}

#[tokio::test]
async fn too_large_messages_of_clients_which_do_not_read_fail_the_session() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, stream) = attach_session(true, false).await?;
            let attach = io.attach().await;
            // A small buffer, so that the message does not fit into it.
            setsockopt(&stream, sockopt::SndBuf, &4096)?;

            // The container keeps writing, until the session waits for
            // the client to read. The client then writes all of its
            // message before it reads, like a client with a single
            // thread.
            let output = {
                let attach = attach.clone();
                tokio::spawn(async move {
                    let data = Bytes::from(vec![b'o'; 64 * 1024]);
                    loop {
                        attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
                        time::sleep(Duration::from_millis(1)).await;
                    }
                })
            };
            time::sleep(Duration::from_secs(1)).await;
            let (mut reader, writer) = send_too_large(stream);
            let written = wait_for(|| writer.is_finished()).await;
            output.abort();
            written.context("the server does not take the rest of the message")?;

            // The output before, then the status and the close.
            let mut status = None;
            loop {
                match server_frame(&mut reader).await? {
                    (8, payload) => {
                        assert_eq!(payload[..2], u16::from(CloseCode::Size).to_be_bytes());
                        break;
                    }
                    (2, payload) if payload.first() == Some(&STREAM_ERR_BYTE) => {
                        status = Some(String::from_utf8_lossy(&payload[1..]).into_owned());
                    }
                    _ => {}
                }
            }
            let status = status.context("no status")?;
            assert!(status.contains(MESSAGE_TOO_LARGE), "{status}");
            too_large_end(reader, writer).await
        })
        .await
}

#[tokio::test]
async fn idle_websocket_sessions_end() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, _server, mut stream) =
                attach_session_with(false, true, true, Some(timeout)).await?;
            let attach = io.attach().await;

            // Stdin keeps the session going.
            for _ in 0..20 {
                time::sleep(timeout / 10).await;
                stream
                    .write_all(&client_frame(0x2, &[STDIN_BYTE, b'x']))
                    .await?;
            }
            assert!(!attach.stdin_closed().is_cancelled());

            // A heartbeat of pings and pongs does not, the session ends
            // like for a client which went away, which closes the stdin
            // once and the connection.
            let pongs = async {
                for opcode in [0x9, 0xa].into_iter().cycle() {
                    time::sleep(timeout / 5).await;
                    if stream.write_all(&client_frame(opcode, &[])).await.is_err() {
                        return;
                    }
                }
            };
            tokio::select! {
                result = time::timeout(4 * timeout, attach.stdin_closed().cancelled()) => result?,
                () = pongs => bail!("connection ended before the stdin got closed"),
            }
            // The client learns about the timeout before the close, which
            // it answers.
            let mut output = vec![];
            loop {
                match server_frame(&mut stream).await? {
                    (0x8, _) => break,
                    (_, payload) => output.extend(payload),
                }
            }
            let output = String::from_utf8_lossy(&output);
            assert!(output.contains("idle timeout"), "{output}");
            stream.write_all(&client_frame(0x8, &[])).await?;
            let mut rest = vec![];
            time::timeout(Duration::from_secs(5), stream.read_to_end(&mut rest)).await??;
            Ok(())
        })
        .await
}

#[tokio::test]
async fn stdin_backed_up_behind_the_container_goes_idle() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, _server, mut stream) =
                attach_session_with(false, true, true, Some(timeout)).await?;
            let attach = io.attach().await;

            // Stdin the container never reads, until the client is not
            // read anymore. The session ends like for a client which went
            // away, although the client still sends.
            let mut frame = vec![STDIN_BYTE];
            frame.resize(60_000, b'x');
            let frame = client_frame(0x2, &frame);
            let send = async { while stream.write_all(&frame).await.is_ok() {} };
            // The stdin gets closed before the connection ends.
            tokio::select! {
                biased;
                result = time::timeout(10 * timeout, attach.stdin_closed().cancelled()) => {
                    result.context("the session did not end")?;
                }
                () = send => bail!("connection ended before the stdin got closed"),
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn clients_which_stop_reading_go_idle() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_secs(1);
            let (io, _server, stream) =
                attach_session_with(false, true, true, Some(timeout)).await?;
            let attach = io.attach().await;

            // The container writes output the client never reads.
            let output = {
                let attach = attach.clone();
                tokio::spawn(async move {
                    let data = Bytes::from(vec![b'o'; 64 * 1024]);
                    loop {
                        attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
                        time::sleep(Duration::from_millis(5)).await;
                    }
                })
            };
            let ended = time::timeout(10 * timeout, attach.stdin_closed().cancelled()).await;
            output.abort();
            drop(stream);
            ended.context("the session of a stuck client did not end")?;
            Ok(())
        })
        .await
}

#[test]
fn only_one_final_status() {
    let idle = || {
        Err(anyhow!(
            "no activity on the connection for the idle timeout"
        ))
    };
    // A status the loop decided on goes out also after the idle timeout,
    // a failure never turns into a success.
    assert_eq!(
        StreamingServer::final_status(idle(), Some(Status::exited(1)), Operation::Exec),
        Some(Status::ExitCode(1))
    );
    assert_eq!(
        StreamingServer::final_status(Ok(()), Some(Status::exited(1)), Operation::Exec),
        Some(Status::ExitCode(1))
    );
    assert!(matches!(
        StreamingServer::final_status(idle(), None, Operation::Exec),
        Some(Status::Error(e)) if e.contains("idle timeout")
    ));
    // A client which went away gets none.
    assert_eq!(
        StreamingServer::final_status(Ok(()), None, Operation::Attach),
        None
    );
}

#[tokio::test]
async fn idle_timeouts_of_stuck_websocket_sessions_send_one_status() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, _server, mut stream) =
                attach_session_with(false, false, true, Some(timeout)).await?;
            let attach = io.attach().await;

            // More output than the connection holds while the client does
            // not read, then the container exits, around the timeout.
            let data = Bytes::from(vec![b'o'; 64 * 1024]);
            for _ in 0..256 {
                attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
            }
            time::sleep(timeout).await;
            attach.write(IOMessage::Done);
            time::sleep(2 * timeout).await;

            // Exactly one status, which is no success.
            let statuses = time::timeout(Duration::from_secs(10), async {
                let mut statuses = vec![];
                loop {
                    let (opcode, payload) = server_frame(&mut stream).await?;
                    match opcode {
                        8 => return anyhow::Ok(statuses),
                        2 if payload.first() == Some(&STREAM_ERR_BYTE) => {
                            statuses.push(String::from_utf8_lossy(&payload[1..]).into_owned());
                        }
                        _ => {}
                    }
                }
            })
            .await??;
            assert_eq!(statuses.len(), 1, "{statuses:?}");
            assert!(!statuses[0].contains("\"Success\""), "{statuses:?}");
            Ok(())
        })
        .await
}

#[tokio::test]
async fn resize_without_terminal_is_ignored() -> Result<()> {
    let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
    SessionInput::try_resize(br#"{"Width":80,"Height":24}"#, &io).await?;

    // Garbage is still an error.
    assert!(SessionInput::try_resize(b"x", &io).await.is_err());
    Ok(())
}

#[test]
fn resize_events_match_fields_like_go() -> Result<()> {
    let size = |width, height| ResizeEvent { width, height };
    for (json, event) in [
        (r#"{"Width":80,"Height":24}"#, size(80, 24)),
        // The Kubernetes client for JavaScript.
        (r#"{"height":24,"width":80}"#, size(80, 24)),
        (r#"{"WIDTH":80,"hEiGhT":24,"Other":1}"#, size(80, 24)),
        (r#"{"Width":80}"#, size(80, 0)),
        (r#"{"Width":80,"Height":null}"#, size(80, 0)),
        ("{}", size(0, 0)),
        // The last of the names which match wins, a null changes
        // nothing.
        (r#"{"width":100,"Width":80,"height":24}"#, size(80, 24)),
        (r#"{"Width":80,"width":100,"height":24}"#, size(100, 24)),
        (r#"{"Width":80,"width":null,"height":24}"#, size(80, 24)),
        // Without a size, which gets ignored.
        ("null", size(0, 0)),
    ] {
        assert_eq!(ResizeEvent::parse(json.as_bytes())?, event, "{json}");
    }
    for json in [
        r#"{"Width":-1}"#,
        r#"{"Height":65536}"#,
        r#"{"Width":"80"}"#,
        "[]",
    ] {
        assert!(ResizeEvent::parse(json.as_bytes()).is_err(), "{json}");
    }
    Ok(())
}

#[tokio::test]
async fn resize_events_without_a_size_are_ignored() -> Result<()> {
    // A terminal which the runtime did not connect fails every resize
    // which reaches it.
    let (io, _child, _token) = attach_child(true)?;
    assert!(
        SessionInput::try_resize(br#"{"width":80,"height":24}"#, &io)
            .await
            .is_err()
    );
    for json in [
        r#"{"width":0,"height":0}"#,
        r#"{"width":100}"#,
        "{}",
        "null",
    ] {
        SessionInput::try_resize(json.as_bytes(), &io).await?;
    }
    Ok(())
}

#[tokio::test]
async fn spdy_stdin_stays_open_until_the_end() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut client) = spdy_attach(false).await?;
            // Client-go takes the end of the stdin stream of the server as
            // the end of stdin.
            let early = async {
                loop {
                    if let Some(Frame::Data {
                        stream_id: 3,
                        fin: true,
                        ..
                    }) = client.frame().await?
                    {
                        return anyhow::Ok(());
                    }
                }
            };
            assert!(
                time::timeout(Duration::from_millis(300), early)
                    .await
                    .is_err()
            );

            client
                .send(Frame::Data {
                    stream_id: 3,
                    fin: false,
                    data: Bytes::from_static(b"in"),
                })
                .await?;
            let attach = io.attach().await;
            let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
            assert_eq!(data, b"in".as_slice());
            Ok(())
        })
        .await
}

#[tokio::test]
async fn late_output_streams_end_instead_of_failing() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            // A container with a terminal, the client asks for stdin
            // without a terminal and opens stderr after the session
            // started, also for versions without resize streams.
            for protocol in ["v2.channel.k8s.io", "v4.channel.k8s.io"] {
                let (io, child, _token) = attach_child(true)?;
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
                let url = server
                    .attach_url(AttachSession {
                        container_id: "id".into(),
                        child,
                        stdin: true,
                        stdout: true,
                        stderr: true,
                        stop_after_stdin_eof: false,
                        idle_timeout: None,
                    })
                    .await?;
                let (response, stream) = upgrade(
                    &server,
                    &url,
                    "POST",
                    &format!(
                        "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                         X-Stream-Protocol-Version: {protocol}\r\n"
                    ),
                )
                .await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                let mut client = Client::new(stream)?;
                for (stream_id, typ) in [(1, "error"), (3, "stdin"), (5, "stdout")] {
                    client.open(stream_id, [("streamType", typ)].into()).await?;
                }
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: Bytes::from_static(b"in"),
                    })
                    .await?;
                let attach = io.attach().await;
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(data, b"in".as_slice());

                client.open(7, [("streamType", "stderr")].into()).await?;
                let frame = client
                    .frame_until(|f| {
                        matches!(
                            f,
                            Frame::RstStream { stream_id: 7, .. }
                                | Frame::Data {
                                    stream_id: 7,
                                    fin: true,
                                    ..
                                }
                        )
                    })
                    .await?;
                assert!(matches!(frame, Frame::Data { .. }), "{protocol}: {frame:?}");
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn idle_spdy_sessions_end() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, _server, mut client) = spdy_attach_with(false, true, Some(timeout)).await?;
            let attach = io.attach().await;

            // Stdin keeps the session going.
            for _ in 0..20 {
                time::sleep(timeout / 10).await;
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: Bytes::from_static(b"x"),
                    })
                    .await?;
            }
            assert!(!attach.stdin_closed().is_cancelled());

            // The pings of the server and their echoes do not. The client
            // learns about the timeout on the error stream, it must not
            // take the end for a success.
            let end = time::timeout(4 * timeout, async {
                let mut status = vec![];
                loop {
                    match client.frame().await? {
                        Some(Frame::Ping { id }) => client.send(Frame::Ping { id }).await?,
                        Some(Frame::Data {
                            stream_id: 1, data, ..
                        }) => status.extend_from_slice(&data),
                        Some(_) => {}
                        None => return anyhow::Ok(status),
                    }
                }
            });
            let status = String::from_utf8(end.await??)?;
            assert!(status.contains("\"status\":\"Failure\""), "{status}");
            assert!(status.contains("idle timeout"), "{status}");
            assert!(attach.stdin_closed().is_cancelled());
            Ok(())
        })
        .await
}

#[tokio::test]
async fn idle_timeouts_of_stuck_spdy_sessions_send_one_status() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, _server, mut client) = spdy_attach_with(false, true, Some(timeout)).await?;
            let attach = io.attach().await;

            // More output than the connection holds while the client does
            // not read, then the container exits, around the timeout.
            let data = Bytes::from(vec![b'o'; 64 * 1024]);
            for _ in 0..256 {
                attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
            }
            time::sleep(timeout).await;
            attach.write(IOMessage::Done);
            time::sleep(2 * timeout).await;

            // Exactly one status on the error stream, which is no success.
            let statuses = time::timeout(Duration::from_secs(10), async {
                let mut statuses = vec![];
                loop {
                    match client.frame().await? {
                        Some(Frame::Data {
                            stream_id: 1, data, ..
                        }) if !data.is_empty() => {
                            statuses.push(String::from_utf8_lossy(&data).into_owned());
                        }
                        Some(_) => {}
                        None => return anyhow::Ok(statuses),
                    }
                }
            })
            .await??;
            assert_eq!(statuses.len(), 1, "{statuses:?}");
            assert!(statuses[0].contains("\"Failure\""), "{statuses:?}");
            Ok(())
        })
        .await
}

/// A client which floods pings and never reads. Its pings count as
/// activity, but their answers back up in the bounded queues of the server,
/// which then stops reading the client, so the session still goes idle. A
/// v1 client, whose stdin the server closes at the start. Over TCP, the
/// socket buffers take hundreds of thousands of pings before the queues fill,
/// which is too slow under qemu. A 4 KiB connection in memory fills after a
/// few hundred.
#[tokio::test]
async fn clients_which_flood_pings_without_reading_go_idle() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, mut client) =
                spdy_attach_duplex(true, Some(timeout), Version::V1, 4096).await?;
            let attach = io.attach().await;
            // Until the server ended the connection.
            let flood = async {
                let mut id: u32 = 1;
                while client.send(Frame::Ping { id }).await.is_ok() {
                    id = id.wrapping_add(2);
                }
                std::future::pending::<()>().await;
            };
            tokio::select! {
                result = time::timeout(10 * timeout, attach.stdin_closed().cancelled()) => {
                    result.context("the session of a flooding client did not end")
                }
                () = flood => unreachable!(),
            }
        })
        .await
}

#[tokio::test]
async fn spdy_streams_after_the_start_get_refused() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut client) = spdy_attach(false).await?;
            // A second stdin stream once the session started.
            client.open(7, [("streamType", "stdin")].into()).await?;
            client
                .frame_until(|f| matches!(f, Frame::RstStream { stream_id: 7, .. }))
                .await?;

            // Its data does not hold up the session.
            for _ in 0..32 {
                client
                    .send(Frame::Data {
                        stream_id: 7,
                        fin: false,
                        data: Bytes::from_static(b"late"),
                    })
                    .await?;
            }
            client
                .send(Frame::Data {
                    stream_id: 3,
                    fin: false,
                    data: Bytes::from_static(b"in"),
                })
                .await?;
            let attach = io.attach().await;
            let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
            assert_eq!(data, b"in".as_slice());
            Ok(())
        })
        .await
}

#[test]
fn resize_events_bypass_pending_stdin() {
    let mut pending = PendingStdin::default();
    let (resize_tx, resize_rx) = watch::channel(None);

    let mut read = |event| StreamingServer::read_event(event, &mut pending, &resize_tx);
    let stdin = || ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"a")));
    assert_eq!(read(stdin()), ControlFlow::Continue(()));
    assert_eq!(
        read(ClientEvent::Resize(Bytes::from_static(b"1"))),
        ControlFlow::Continue(())
    );
    assert_eq!(
        read(ClientEvent::Resize(Bytes::from_static(b"2"))),
        ControlFlow::Continue(())
    );

    // Only the latest size is kept, the stdin waits on its own.
    assert_eq!(resize_rx.borrow().as_deref(), Some(&b"2"[..]));
    assert_eq!(pending.pop(), Stdin::Data(Bytes::from_static(b"a")));
    assert!(pending.is_empty());

    let close = StreamingServer::read_event(ClientEvent::Closed, &mut pending, &resize_tx);
    assert_eq!(close, ControlFlow::Break(EventEnd::Closed));
    let too_large = StreamingServer::read_event(ClientEvent::TooLarge, &mut pending, &resize_tx);
    assert_eq!(too_large, ControlFlow::Break(EventEnd::TooLarge));
}

#[test]
fn pending_stdin_is_bounded() {
    let mut pending = PendingStdin::default();
    let len = PENDING_STDIN_MAX_BYTES - 64 * STDIN_ENTRY_OVERHEAD;
    pending.push(Stdin::Data(Bytes::from(vec![0; len])));
    assert_eq!(
        pending.len(),
        PENDING_STDIN_MAX_BYTES - 48 * STDIN_ENTRY_OVERHEAD
    );
    assert!(!pending.is_full());
    pending.push(Stdin::Close);
    assert!(!pending.is_full());
    // Nothing takes stdin after the close, it does not count.
    pending.push(Stdin::Data(Bytes::from(vec![0; 48 * STDIN_ENTRY_OVERHEAD])));
    assert!(!pending.is_full());

    let mut taken = 0;
    while !pending.is_empty() {
        match pending.pop() {
            Stdin::Data(data) => {
                assert!(data.len() <= STDIN_CHUNK_SIZE);
                taken += data.len();
            }
            Stdin::Close => assert!(pending.is_empty()),
        }
    }
    assert_eq!(taken, len);
    assert_eq!(pending.len(), 0);

    pending.clear();
    assert!(pending.is_empty());
}

#[test]
fn pending_stdin_drops_what_carries_nothing() {
    let mut pending = PendingStdin::default();
    pending.push(Stdin::Data(Bytes::new()));
    assert!(pending.is_empty());

    // Only the first close counts, also without a container which takes
    // the stdin.
    for _ in 0..1000 {
        pending.push(Stdin::Close);
    }
    assert_eq!(pending.len(), STDIN_ENTRY_OVERHEAD);
    assert_eq!(pending.pop(), Stdin::Close);
    assert!(pending.is_empty());
    pending.push(Stdin::Close);
    assert!(pending.is_empty());
}

#[test]
#[expect(clippy::cast_possible_truncation, reason = "a wrapping byte pattern")]
fn small_stdin_gets_merged() {
    let mut pending = PendingStdin::default();
    for i in 0..2 * STDIN_CHUNK_SIZE {
        pending.push(Stdin::Data(Bytes::from(vec![i as u8])));
    }
    // Two full entries, not one per byte.
    assert_eq!(pending.len(), 2 * (STDIN_CHUNK_SIZE + STDIN_ENTRY_OVERHEAD));
    pending.push(Stdin::Close);

    let mut data = vec![];
    while let Stdin::Data(chunk) = pending.pop() {
        data.extend_from_slice(&chunk);
    }
    assert_eq!(
        data,
        (0..2 * STDIN_CHUNK_SIZE)
            .map(|i| i as u8)
            .collect::<Vec<_>>()
    );
    assert!(pending.is_empty());
}

#[test]
fn small_stdin_does_not_keep_the_buffer_of_the_tail() {
    let mut pending = PendingStdin::default();
    pending.push(Stdin::Data(Bytes::from(vec![0; STDIN_MERGE_SIZE - 1])));
    pending.push(Stdin::Data(Bytes::from(vec![0; STDIN_MERGE_SIZE - 1])));
    // More than the tail holds, the merged stdin becomes a chunk.
    pending.push(Stdin::Data(Bytes::from_static(b"abc")));
    let Stdin::Data(merged) = pending.pop() else {
        panic!("no data");
    };
    let Stdin::Data(small) = pending.pop() else {
        panic!("no data");
    };
    assert_eq!(merged.len(), 2 * (STDIN_MERGE_SIZE - 1));
    assert_eq!(small, Bytes::from_static(b"abc"));
    // The stdin owns all of its buffer, see `Bytes::try_into_mut`.
    for data in [merged, small] {
        let len = data.len();
        assert!(data.try_into_mut().is_ok_and(|data| data.capacity() == len));
    }
}

#[test]
fn large_stdin_gets_split_without_copying() {
    let mut pending = PendingStdin::default();
    pending.push(Stdin::Data(Bytes::from_static(b"a")));
    let data = Bytes::from(vec![1; 2 * STDIN_CHUNK_SIZE + 1]);
    pending.push(Stdin::Data(data.clone()));

    // The small stdin before it keeps its place.
    assert_eq!(pending.pop(), Stdin::Data(Bytes::from_static(b"a")));
    let mut offset = 0;
    for len in [STDIN_CHUNK_SIZE, STDIN_CHUNK_SIZE, 1] {
        let Stdin::Data(chunk) = pending.pop() else {
            panic!("no data");
        };
        assert_eq!(chunk.len(), len);
        assert_eq!(chunk.as_ptr(), data[offset..].as_ptr());
        offset += len;
    }
    assert!(pending.is_empty());
}
