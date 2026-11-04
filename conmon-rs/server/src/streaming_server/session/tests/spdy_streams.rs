//! The streams of a SPDY session.

use super::*;

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
            let attach = io.attach().clone();
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
                let server = StreamingServer::default();
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
                let attach = io.attach().clone();
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
            let attach = io.attach().clone();
            let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
            assert_eq!(data, b"in".as_slice());
            Ok(())
        })
        .await
}
