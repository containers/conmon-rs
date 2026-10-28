//! Attach sessions, which connect their client to the container process.

use super::{
    StreamingServer,
    session::{ClientRx, InputEvent, SessionInput},
};
use crate::{
    attach::SharedContainerAttach,
    child_reaper::ReapableChild,
    container_io::{Drain, Message as IOMessage, Pipe},
    remotecommand::{MESSAGE_TOO_LARGE, Output, STDERR_BYTE, STDOUT_BYTE, Status},
};
use anyhow::{Result, bail};
use tokio::{
    sync::broadcast::{self, error::RecvError},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

#[derive(Debug)]
/// Required attach session data.
pub struct AttachSession {
    pub container_id: Box<str>,
    pub child: ReapableChild,
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub stop_after_stdin_eof: bool,
    pub idle_timeout: Option<Duration>,
}

impl StreamingServer {
    /// The attach specific read/write loop. `status` gets the success once
    /// the session ended, unless the client went away, which goes out after
    /// the loop.
    pub(super) async fn attach_loop(
        session: AttachSession,
        sender: &mut Output,
        client: ClientRx,
        status: &mut Option<Status>,
    ) -> Result<()> {
        // Extract values we need for the I/O loop
        let stdin_enabled = session.stdin;
        let stdout_enabled = session.stdout;
        let stderr_enabled = session.stderr;
        let stop_after_stdin_eof = session.stop_after_stdin_eof;
        let io = session.child.io().clone();
        let token = session.child.token().clone();

        // Explicitly drop session to free the ReapableChild before entering the I/O loop
        drop(session);

        // Subscribe to the attach broadcast rather than the stdio channels. The
        // broadcast only carries output produced while someone is attached,
        // whereas the stdio channels replay buffered history and are closed
        // for created containers anyway.
        let attach = io.attach().await;
        let terminal = attach.is_terminal();
        // Without stdin, there is none to wait for.
        let mut input = SessionInput::new(io, attach.clone(), client, stdin_enabled);

        // With stdin once, the end of the session closes the container stdin,
        // also if the client went away without closing it, like the first
        // session ending does with StdinOnce in Kubernetes.
        let mut teardown = AttachTeardown {
            attach: attach.clone(),
            token: token.clone(),
            close_stdin: stdin_enabled && stop_after_stdin_eof,
        };

        // A single `Done` follows once all pipes ended. It may have been sent
        // before subscribing, then only forward what is still queued.
        let (mut attach_rx, mut done) = attach.subscribe();
        if done {
            while let Ok(message) = attach_rx.try_recv() {
                Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
            }
        }

        // Until nothing is left to forward in either direction.
        while !(done && !input.stdin_open) {
            tokio::select! {
                res = attach_rx.recv(), if !done => match res {
                    Ok(IOMessage::Done) => done = true,
                    Ok(message) => {
                        Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
                    }
                    Err(RecvError::Lagged(n)) => warn!("Attach client too slow, skipped {n} messages"),
                    Err(RecvError::Closed) => {
                        debug!("Exiting streaming attach because container I/O is gone");
                        break
                    }
                },

                event = input.next() => match event {
                    InputEvent::Resize(data) => input.resize(&data).await,
                    InputEvent::Ping => sender.ping().await?,
                    // Without a terminal and stdin once, that ends the
                    // session, like for CRI-O with conmon. Otherwise keep
                    // forwarding the output until it ends, and with stdin
                    // once close the container stdin.
                    InputEvent::StdinClosed if !terminal && !stop_after_stdin_eof => break,
                    InputEvent::StdinClosed => {
                        if stop_after_stdin_eof {
                            teardown.close_stdin = false;
                            input.close_container_stdin().await;
                        }
                    }
                    // The client closed the connection, everything it sent
                    // reached the container. Nobody gets a status anymore.
                    InputEvent::Ended => return Ok(()),
                    InputEvent::TooLarge => bail!(MESSAGE_TOO_LARGE),
                },

                () = token.cancelled() => {
                    debug!("Exiting streaming attach because token cancelled");
                    if !done {
                        Self::drain_attach(&mut attach_rx, stdout_enabled, stderr_enabled, sender).await?;
                    }
                    break
                }
            }
        }

        *status = Some(Status::Success);
        Ok(())
    }

    /// Forward a single output message to the WebSocket client.
    async fn forward_output(
        message: IOMessage,
        stdout_enabled: bool,
        stderr_enabled: bool,
        sender: &mut Output,
    ) -> Result<()> {
        match message {
            IOMessage::Data(data, Pipe::StdOut) if stdout_enabled => {
                sender.data(STDOUT_BYTE, &data).await
            }
            IOMessage::Data(data, Pipe::StdErr) if stderr_enabled => {
                sender.data(STDERR_BYTE, &data).await
            }
            IOMessage::Data(..) | IOMessage::Done => Ok(()),
        }
    }

    /// Forward the output which is still queued or arrives shortly after the
    /// container exited, until the output ended or the [`Drain`] gives up.
    async fn drain_attach(
        attach_rx: &mut broadcast::Receiver<IOMessage>,
        stdout_enabled: bool,
        stderr_enabled: bool,
        sender: &mut Output,
    ) -> Result<()> {
        let mut drain = Drain::default();
        while let Some(message) = drain.recv_output(attach_rx, true).await {
            Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
        }
        Ok(())
    }
}

/// Closes the container stdin when an attach session with stdin once ends
/// while the container runs, also if the client went away without closing
/// its stdin and the loop got aborted.
struct AttachTeardown {
    attach: SharedContainerAttach,

    /// Cancelled once the container exited.
    token: CancellationToken,

    /// Whether the container stdin still has to be closed.
    close_stdin: bool,
}

impl Drop for AttachTeardown {
    fn drop(&mut self) {
        if self.close_stdin && !self.token.is_cancelled() {
            debug!("Closing the container stdin after the end of the attach session");
            self.attach.close_container_stdin();
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::{
        container_io::{Message as IOMessage, Pipe},
        remotecommand::{CLOSE_BYTE, STDIN_BYTE, STDOUT_BYTE, STREAM_ERR_BYTE},
        spdy::testing::Frame,
        streaming_server::{
            AttachSession, StreamingServer,
            session::{PENDING_STDIN_MAX_BYTES, PING_INTERVAL},
            testing::*,
        },
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
                assert!(!io.attach().await.stdin_closed().is_cancelled());
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn attach_closes_stdin_once_on_disconnect() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, stream) = attach_session(true, true).await?;
                let attach = io.attach().await;
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
                let attach = io.attach().await;

                // More stdin than the session holds while nothing reads it,
                // so the socket is not read anymore.
                let mut frame = vec![STDIN_BYTE];
                frame.resize(60_000, b'x');
                let frame = client_frame(0x2, &frame);
                for _ in 0..PENDING_STDIN_MAX_BYTES / 60_000 + 40 {
                    stream.write_all(&frame).await?;
                }
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
                let attach = io.attach().await;
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
                let attach = io.attach().await;
                stream
                    .write_all(&client_frame(0x2, &[CLOSE_BYTE, STDIN_BYTE]))
                    .await?;

                // The output still reaches the client.
                time::timeout(Duration::from_secs(5), async {
                    while !attach.has_readers() {
                        time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await?;
                attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                assert_eq!(
                    server_frame(&mut stream).await?,
                    (0x2, vec![STDOUT_BYTE, b'o', b'u', b't'])
                );
                drop(stream);
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
                let attach = io.attach().await;
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
                    let attach = io.attach().await;
                    let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                    assert_eq!(data, b"in".as_slice(), "{version}");

                    attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                    let frame = client
                        .frame_until(|f| matches!(f, Frame::Data { stream_id: 5, data, .. } if !data.is_empty()))
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

                let attach = io.attach().await;
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
                    task::spawn_local(
                        async move { while let Ok(Some(_)) = client.frame().await {} },
                    );

                let attach = io.attach().await;
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
                assert!(!io.attach().await.stdin_closed().is_cancelled());
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
                let attach = io.attach().await;
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
                let attach = io.attach().await;
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
                let attach = io.attach().await;
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
                let attach = io.attach().await;

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
                let mut server = StreamingServer::default();
                server.start_if_required().await?;
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
                    let (response, stream) = upgrade(
                        &server,
                        &url,
                        "GET",
                        &format!(
                            "Upgrade: websocket\r\nConnection: Upgrade\r\n\
                             Sec-WebSocket-Version: 13\r\n\
                             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                             Sec-WebSocket-Protocol: {PROTOCOL_V5}\r\n"
                        ),
                    )
                    .await?;
                    assert!(response.starts_with("http/1.1 101"), "{response}");
                    streams.push(stream);
                }
                let attach = io.attach().await;

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
                let attach = io.attach().await;
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
                let (_io, _server, mut stream, token, response) =
                    websocket_attach(PROTOCOL_V5).await?;
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
                let attach = io.attach().await;
                attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                assert_eq!(server_frame(&mut stream).await?, (0x2, b"\x01out".to_vec()));

                // A success has no status before v4.
                token.cancel();
                assert_eq!(server_frame(&mut stream).await?.0, 0x8);
                Ok(())
            })
            .await
    }
}
