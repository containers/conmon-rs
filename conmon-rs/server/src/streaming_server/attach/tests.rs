use crate::{
    container_io::{Message as IOMessage, Pipe},
    streaming_server::{
        AttachSession, StreamingServer,
        remotecommand::{CLOSE_BYTE, STDIN_BYTE, STDOUT_BYTE, STREAM_ERR_BYTE},
        session::{PENDING_STDIN_MAX_BYTES, PING_INTERVAL},
        spdy::testing::Frame,
        testing::*,
    },
    test_util::wait_for,
};
use anyhow::Result;
use bytes::Bytes;
use data_encoding::BASE64;
use tokio::{
    io::AsyncWriteExt,
    task::{self, LocalSet},
    time::{self, Duration},
};

#[tokio::test]
async fn attach_ends_with_success_on_stdin_close() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut stream) = attach_session(true, false).await?;
            stream
                .write_all(&client_frame(0x2, &[CLOSE_BYTE, STDIN_BYTE]))
                .await?;

            assert_eq!(
                server_frame(&mut stream).await?,
                (
                    0x2,
                    [
                        &[STREAM_ERR_BYTE][..],
                        br#"{"metadata":{},"status":"Success"}"#
                    ]
                    .concat()
                )
            );
            assert_eq!(server_frame(&mut stream).await?.0, 0x8);
            // Without stdin once, the container stdin stays open.
            assert!(!io.attach().stdin_closed().is_cancelled());
            Ok(())
        })
        .await
}

#[tokio::test]
async fn attach_closes_stdin_once_on_disconnect() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, stream) = attach_session(true, true).await?;
            let attach = io.attach().clone();
            drop(stream);
            time::timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
            Ok(())
        })
        .await
}

#[tokio::test]
async fn attach_ends_on_disconnect_while_stdin_waits() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut stream) = attach_session(true, true).await?;
            let attach = io.attach().clone();

            // More stdin than the session holds while nothing reads it,
            // so the socket is not read anymore.
            let mut frame = vec![STDIN_BYTE];
            frame.resize(60_000, b'x');
            let frame = client_frame(0x2, &frame);
            for _ in 0..PENDING_STDIN_MAX_BYTES / 60_000 + 40 {
                stream.write_all(&frame).await?;
            }
            // Nothing tells that the session goes on, one which ends later
            // passes.
            time::sleep(Duration::from_millis(200)).await;
            assert!(!attach.stdin_closed().is_cancelled());
            drop(stream);

            // The session notices and ends, which closes the stdin once.
            time::timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
            Ok(())
        })
        .await
}

#[tokio::test]
async fn terminal_attach_gets_eof_on_stdin_close() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut stream) = attach_session_with(true, true, true, None).await?;
            let mut frames = client_frame(0x2, &[STDIN_BYTE, b'a']);
            frames.extend(client_frame(0x2, &[CLOSE_BYTE, STDIN_BYTE]));
            stream.write_all(&frames).await?;

            // The end of file character follows the stdin, like a user
            // pressing Ctrl-D.
            let attach = io.attach().clone();
            for expected in [&b"a"[..], &[0x04]] {
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(data, expected);
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn attach_without_stdin_ignores_stdin_close() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut stream) = attach_session(false, true).await?;
            let attach = io.attach().clone();
            stream
                .write_all(&client_frame(0x2, &[CLOSE_BYTE, STDIN_BYTE]))
                .await?;

            // The output still reaches the client.
            wait_for(|| attach.has_readers()).await?;
            attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
            assert_eq!(
                server_frame(&mut stream).await?,
                (0x2, vec![STDOUT_BYTE, b'o', b'u', b't'])
            );
            drop(stream);
            // Nothing tells that the session ended, a close which comes
            // later passes.
            time::sleep(Duration::from_millis(50)).await;
            assert!(!attach.stdin_closed().is_cancelled());
            Ok(())
        })
        .await
}

#[tokio::test]
async fn spdy_attach_forwards_stdin_and_output() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut client) = spdy_attach(false).await?;
            client
                .send(Frame::Data {
                    stream_id: 3,
                    fin: false,
                    data: Bytes::from_static(b"in"),
                })
                .await?;
            let attach = io.attach().clone();
            let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
            assert_eq!(data, b"in".as_slice());

            attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
            let frame = client
                .frame_until(|f| matches!(f, Frame::Data { stream_id: 5, .. }))
                .await?;
            assert_eq!(
                frame,
                Frame::Data {
                    stream_id: 5,
                    fin: false,
                    data: Bytes::from_static(b"out")
                }
            );
            Ok(())
        })
        .await
}

#[tokio::test]
async fn spdy_attach_with_every_version() -> Result<()> {
    for version in [
        "channel.k8s.io",
        "v2.channel.k8s.io",
        "v3.channel.k8s.io",
        "v4.channel.k8s.io",
    ] {
        LocalSet::new()
            .run_until(async {
                let headers = format!(
                    "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                     X-Stream-Protocol-Version: {version}\r\n"
                );
                let (io, _server, mut client) =
                    spdy_attach_version(false, false, None, &headers).await?;
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: Bytes::from_static(b"in"),
                    })
                    .await?;
                let attach = io.attach().clone();
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(data, b"in".as_slice(), "{version}");

                attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                let frame = client
                    .frame_until(
                        |f| matches!(f, Frame::Data { stream_id: 5, data, .. } if !data.is_empty()),
                    )
                    .await?;
                assert!(
                    matches!(frame, Frame::Data { data, .. } if data == "out"),
                    "{version}"
                );

                // Closing stdin ends the session, only v4 tells about the
                // success.
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: true,
                        data: Bytes::new(),
                    })
                    .await?;
                let mut status = vec![];
                while let Some(frame) =
                    time::timeout(Duration::from_secs(5), client.frame()).await??
                {
                    if let Frame::Data {
                        stream_id: 1, data, ..
                    } = frame
                    {
                        status.extend_from_slice(&data);
                    }
                }
                let expected: &[u8] = if version.starts_with("v4") {
                    br#"{"metadata":{},"status":"Success"}"#
                } else {
                    b""
                };
                assert_eq!(status, expected, "{version}");
                Ok::<_, anyhow::Error>(())
            })
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn spdy_stdin_before_the_close_reaches_the_container() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut client) = spdy_attach(false).await?;
            for (fin, data) in [(false, "a"), (true, "tail")] {
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin,
                        data: Bytes::from_static(data.as_bytes()),
                    })
                    .await?;
            }
            drop(client);

            let attach = io.attach().clone();
            let mut stdin = vec![];
            while stdin.len() < 5 {
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                stdin.extend_from_slice(&data);
            }
            assert_eq!(stdin, b"atail");
            Ok(())
        })
        .await
}

#[tokio::test]
async fn spdy_stdin_before_the_close_of_an_older_session_reaches_the_container() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut client) = spdy_attach(false).await?;
            // The session pings the client while its input waits, which
            // must not end it once the connection ended after stdin.
            time::sleep(3 * PING_INTERVAL).await;
            for (fin, data) in [(false, "a"), (true, "tail")] {
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin,
                        data: Bytes::from_static(data.as_bytes()),
                    })
                    .await?;
            }
            client.io().shutdown().await?;
            // A client which closes with frames unread resets the
            // connection, so keep reading.
            let reader =
                task::spawn_local(async move { while let Ok(Some(_)) = client.frame().await {} });

            let attach = io.attach().clone();
            let mut stdin = vec![];
            while stdin.len() < 5 {
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                stdin.extend_from_slice(&data);
            }
            assert_eq!(stdin, b"atail");
            reader.abort();
            Ok(())
        })
        .await
}

#[tokio::test]
async fn spdy_attach_ends_with_success_on_stdin_close() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut client) = spdy_attach(false).await?;
            client
                .send(Frame::Data {
                    stream_id: 3,
                    fin: true,
                    data: Bytes::new(),
                })
                .await?;

            let status = client
                .frame_until(
                    |f| matches!(f, Frame::Data { stream_id: 1, data, .. } if !data.is_empty()),
                )
                .await?;
            assert_eq!(
                status,
                Frame::Data {
                    stream_id: 1,
                    fin: false,
                    data: Bytes::from_static(br#"{"metadata":{},"status":"Success"}"#)
                }
            );
            // The output and the error stream end, then the connection.
            let mut ended = vec![];
            loop {
                match client.frame().await? {
                    Some(Frame::Data {
                        stream_id,
                        fin: true,
                        ..
                    }) => ended.push(stream_id),
                    Some(_) => {}
                    None => break,
                }
            }
            ended.sort();
            // Stdin ends only with the session.
            assert_eq!(ended, [1, 3, 5]);
            // Without stdin once, the container stdin stays open.
            assert!(!io.attach().stdin_closed().is_cancelled());
            Ok(())
        })
        .await
}

#[tokio::test]
async fn spdy_attach_to_a_terminal_without_tty() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            // The client does not ask for a terminal, it opens no resize
            // stream, the session starts nevertheless.
            let (io, _server, mut client) = spdy_attach_with(true, false, None).await?;
            client
                .send(Frame::Data {
                    stream_id: 3,
                    fin: false,
                    data: Bytes::from_static(b"in"),
                })
                .await?;
            let attach = io.attach().clone();
            let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
            assert_eq!(data, b"in".as_slice());

            // A resize stream may still come, a second one not.
            client.open(7, [("streamType", "resize")].into()).await?;
            client.open(9, [("streamType", "resize")].into()).await?;
            let reset = client
                .frame_until(|f| matches!(f, Frame::RstStream { .. }))
                .await?;
            assert!(
                matches!(reset, Frame::RstStream { stream_id: 9, .. }),
                "{reset:?}"
            );
            Ok(())
        })
        .await
}

#[tokio::test]
async fn websocket_attach_closes_stdin_once_after_an_early_disconnect() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, stream, _token, response) =
                websocket_attach_with("channel.k8s.io", true).await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            let attach = io.attach().clone();
            // Before the client reads the announce of the session.
            drop(stream);
            time::timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
            Ok(())
        })
        .await
}

#[tokio::test]
async fn spdy_attach_closes_stdin_once() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut client) = spdy_attach(true).await?;
            let attach = io.attach().clone();
            client
                .send(Frame::Data {
                    stream_id: 3,
                    fin: true,
                    data: Bytes::new(),
                })
                .await?;
            time::timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
            Ok(())
        })
        .await
}

#[tokio::test]
async fn spdy_attach_ends_on_disconnect_while_stdin_waits() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut client) = spdy_attach(true).await?;
            let attach = io.attach().clone();

            // More stdin than the session holds while nothing reads it,
            // so the stream queue fills and the connection is not read
            // anymore, until the client cannot write anymore either.
            let frame = Bytes::from(vec![b'x'; 32 * 1024]);
            let fill = async {
                while client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: frame.clone(),
                    })
                    .await
                    .is_ok()
                {}
            };
            assert!(time::timeout(Duration::from_secs(1), fill).await.is_err());
            assert!(!attach.stdin_closed().is_cancelled());
            drop(client);

            // The pings of the session notice and end it, which closes
            // the stdin once.
            time::timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
            Ok(())
        })
        .await
}

#[tokio::test]
async fn concurrent_attaches_share_the_container() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, child, token) = attach_child(false)?;
            let server = StreamingServer::default();
            let mut streams = vec![];
            for _ in 0..2 {
                let url = server
                    .attach_url(AttachSession {
                        container_id: "id".into(),
                        child: child.clone(),
                        stdin: true,
                        stdout: true,
                        stderr: false,
                        stop_after_stdin_eof: false,
                        idle_timeout: None,
                    })
                    .await?;
                let (response, stream) =
                    upgrade(&server, &url, "GET", &websocket_headers(PROTOCOL_V5)).await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                streams.push(stream);
            }
            let attach = io.attach().clone();

            // The stdin of both reaches the container.
            for (stream, data) in streams.iter_mut().zip([b"a", b"b"]) {
                stream
                    .write_all(&client_frame(0x2, &[&[STDIN_BYTE], &data[..]].concat()))
                    .await?;
                let read = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(read, &data[..]);
            }

            // Both get the output, then the status and the close once the
            // container exited.
            attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
            attach.write(IOMessage::Done);
            token.cancel();
            for stream in &mut streams {
                assert_eq!(
                    server_frame(stream).await?,
                    (0x2, [&[STDOUT_BYTE][..], b"out"].concat())
                );
                assert_eq!(
                    server_frame(stream).await?,
                    (
                        0x2,
                        [
                            &[STREAM_ERR_BYTE][..],
                            br#"{"metadata":{},"status":"Success"}"#
                        ]
                        .concat()
                    )
                );
                assert_eq!(server_frame(stream).await?.0, 0x8);
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn base64_websocket_attach() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut stream, token, response) =
                websocket_attach("v4.base64.channel.k8s.io").await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            assert!(
                response.contains("sec-websocket-protocol: v4.base64.channel.k8s.io"),
                "{response}"
            );
            // An empty message on stdout tells the client that the
            // session started.
            assert_eq!(server_frame(&mut stream).await?, (0x1, b"1".to_vec()));

            let stdin = format!("0{}", BASE64.encode(b"in"));
            stream
                .write_all(&client_frame(0x1, stdin.as_bytes()))
                .await?;
            let attach = io.attach().clone();
            let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
            assert_eq!(data, b"in".as_slice());

            attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
            assert_eq!(
                server_frame(&mut stream).await?,
                (0x1, format!("1{}", BASE64.encode(b"out")).into_bytes())
            );

            // The v4 status is JSON, encoded like the output.
            token.cancel();
            assert_eq!(
                server_frame(&mut stream).await?,
                (
                    0x1,
                    format!(
                        "3{}",
                        BASE64.encode(br#"{"metadata":{},"status":"Success"}"#)
                    )
                    .into_bytes()
                )
            );
            assert_eq!(server_frame(&mut stream).await?.0, 0x8);
            Ok(())
        })
        .await
}

#[tokio::test]
async fn websocket_attach_waits_for_the_close_answer() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (_io, _server, mut stream, token, response) = websocket_attach(PROTOCOL_V5).await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            token.cancel();
            assert_eq!(server_frame(&mut stream).await?.1[0], STREAM_ERR_BYTE);
            assert_eq!(server_frame(&mut stream).await?.0, 0x8);
            answer_close(&mut stream).await
        })
        .await
}

#[tokio::test]
async fn v1_websocket_attach() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut stream, token, response) =
                websocket_attach("channel.k8s.io").await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            assert_eq!(server_frame(&mut stream).await?, (0x2, vec![STDOUT_BYTE]));

            // There is no close signal before v5, the session goes on.
            stream
                .write_all(&client_frame(0x2, &[CLOSE_BYTE, STDIN_BYTE]))
                .await?;
            let attach = io.attach().clone();
            attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
            assert_eq!(server_frame(&mut stream).await?, (0x2, b"\x01out".to_vec()));

            // A success has no status before v4.
            token.cancel();
            assert_eq!(server_frame(&mut stream).await?.0, 0x8);
            Ok(())
        })
        .await
}
