use super::{spdy::ResizeEvents, status::ErrorMessage, *};
use crate::streaming_server::{
    spdy::{
        self, Connection,
        testing::{Client, Frame},
    },
    stream_protocol::first_supported,
};
use tokio::{
    io::{AsyncReadExt, duplex},
    time::{self, Duration},
};

#[test]
fn websocket_protocols_get_negotiated() {
    let negotiate = |requested: &[&str]| Framing::negotiate(requested.iter().copied());
    let v1 = Framing {
        version: Version::V1,
        base64: false,
    };
    assert_eq!(negotiate(&[]), Some((v1, None)));
    assert_eq!(
        negotiate(&["v5.channel.k8s.io", "v4.channel.k8s.io"]),
        Some((Framing::V5, Some("v5.channel.k8s.io")))
    );
    // The order of the client decides.
    assert_eq!(
        negotiate(&["unknown", "v4.base64.channel.k8s.io", "v5.channel.k8s.io"]),
        Some((
            Framing {
                version: Version::V4,
                base64: true
            },
            Some("v4.base64.channel.k8s.io")
        ))
    );
    assert_eq!(
        negotiate(&["base64.channel.k8s.io"]),
        Some((
            Framing {
                version: Version::V1,
                base64: true
            },
            Some("base64.channel.k8s.io")
        ))
    );
    assert_eq!(negotiate(&["v3.channel.k8s.io"]), None);
}

#[test]
fn spdy_versions() {
    let version = |protocol| first_supported([protocol], &SPDY_PROTOCOLS).map(|(v, _)| v);
    assert_eq!(version("v4.channel.k8s.io"), Some(Version::V4));
    assert_eq!(version("channel.k8s.io"), Some(Version::V1));
    assert_eq!(version("v5.channel.k8s.io"), None);
    assert!(Version::V3.spdy_resize());
    assert!(!Version::V2.spdy_resize());
}

#[test]
fn v5_events() {
    let event = |data: &'static [u8]| Framing::V5.event(Message::Binary(Bytes::from_static(data)));
    assert_eq!(
        event(&[STDIN_BYTE, b'l', b's']),
        Some(ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"ls"))))
    );
    // Empty stdin carries nothing.
    assert_eq!(event(&[STDIN_BYTE]), None);
    assert_eq!(
        event(&[CLOSE_BYTE, STDIN_BYTE]),
        Some(ClientEvent::Stdin(Stdin::Close))
    );
    // A close needs the stdin channel, others cannot be closed.
    assert_eq!(event(&[CLOSE_BYTE]), None);
    assert_eq!(event(&[CLOSE_BYTE, RESIZE_BYTE]), None);
    assert_eq!(event(&[CLOSE_BYTE, STDIN_BYTE, STDIN_BYTE]), None);
    assert_eq!(
        event(&[RESIZE_BYTE, b'{', b'}']),
        Some(ClientEvent::Resize(Bytes::from_static(b"{}")))
    );
    // Unknown and empty messages are ignored.
    assert_eq!(event(&[42, b'x']), None);
    assert_eq!(event(&[]), None);
    // Text messages carry the same, like for Kubernetes.
    assert_eq!(
        Framing::V5.event(Message::Text("\0ls".into())),
        Some(ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"ls"))))
    );
    assert_eq!(
        Framing::V5.event(Message::Close(None)),
        Some(ClientEvent::Closed)
    );
}

#[test]
fn older_events_have_no_close_signal() {
    let binary = Framing {
        version: Version::V4,
        base64: false,
    };
    let event = |data: &'static [u8]| binary.event(Message::Binary(Bytes::from_static(data)));
    assert_eq!(
        event(b"\x00ls"),
        Some(ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"ls"))))
    );
    assert_eq!(
        event(b"\x04{}"),
        Some(ClientEvent::Resize(Bytes::from_static(b"{}")))
    );
    // The close signal of v5, the output channels and empty stdin are
    // ignored.
    assert_eq!(event(&[CLOSE_BYTE, STDIN_BYTE]), None);
    assert_eq!(event(b"\x01out"), None);
    assert_eq!(event(&[STDIN_BYTE]), None);

    let base64 = Framing {
        version: Version::V1,
        base64: true,
    };
    assert_eq!(
        base64.event(Message::Text("0bHM=".into())),
        Some(ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"ls"))))
    );
    assert_eq!(
        base64.event(Message::Binary(Bytes::from_static(b"4e30="))),
        Some(ClientEvent::Resize(Bytes::from_static(b"{}")))
    );
    assert_eq!(base64.event(Message::Text("0!".into())), None);
    assert_eq!(base64.event(Message::Text("0".into())), None);
}

#[test]
fn statuses_depend_on_the_version() -> Result<()> {
    assert_eq!(
        Status::Success.encode(Version::V4)?.as_deref(),
        Some(SUCCESS_STATUS)
    );
    assert_eq!(Status::Success.encode(Version::V3)?, None);
    assert_eq!(
        Status::ExitCode(137).encode(Version::V5)?,
        Some(serde_json::to_vec(&ErrorMessage::new(137))?)
    );
    assert_eq!(
        Status::ExitCode(137).encode(Version::V2)?.as_deref(),
        Some(&b"command terminated with non-zero exit code: exit status 137"[..])
    );
    assert_eq!(
        Status::Error("failed".into())
            .encode(Version::V1)?
            .as_deref(),
        Some(&b"failed"[..])
    );
    Ok(())
}

#[test]
fn exit_statuses() -> Result<()> {
    assert_eq!(
        Status::exited(0).encode(Version::V5)?.as_deref(),
        Some(SUCCESS_STATUS)
    );
    assert_eq!(
        Status::exited(137).encode(Version::V5)?,
        Some(serde_json::to_vec(&ErrorMessage::new(137))?)
    );
    // Clients cannot parse a negative exit code, it is an error instead.
    assert_eq!(
        Status::exited(-3).encode(Version::V4)?.as_deref(),
        Some(
            &br#"{"status":"Failure","message":"Internal error occurred: error executing command in container: unable to get the exit code of the command (-3)","reason":"InternalError","code":500}"#[..]
        )
    );
    assert_eq!(
        Status::exited(-1).encode(Version::V2)?.as_deref(),
        Some(
            &b"Internal error occurred: error executing command in container: unable to get the exit code of the command (-1)"[..]
        )
    );
    Ok(())
}

/// A failed session gets the status of k8s.io/cri-streaming, which has no
/// details, unlike the one of an exit code.
#[test]
fn failures_like_cri_streaming() -> Result<()> {
    assert_eq!(
        Status::failed(Operation::Attach, "gone").encode(Version::V5)?.as_deref(),
        Some(
            &br#"{"status":"Failure","message":"Internal error occurred: error attaching to container: gone","reason":"InternalError","code":500}"#[..]
        )
    );
    Ok(())
}

#[test]
fn success_status() {
    assert_eq!(SUCCESS_STATUS, b"{\"metadata\":{},\"status\":\"Success\"}");
    assert_eq!(
        Framing::V5.message(STREAM_ERR_BYTE, SUCCESS_STATUS),
        Message::Binary(Bytes::from_static(
            b"\x03{\"metadata\":{},\"status\":\"Success\"}"
        ))
    );
}

#[test]
fn exit_code_error_message() -> Result<()> {
    assert_eq!(
        serde_json::to_string(&ErrorMessage::new(137))?,
        r#"{"status":"Failure","message":"command terminated with non-zero exit code: exit status 137","reason":"NonZeroExitCode","details":{"causes":[{"reason":"ExitCode","message":"137"}]}}"#
    );
    Ok(())
}

#[test]
fn resize_streams_get_split_into_events() {
    let mut input = ResizeEvents::default();
    // Go encodes one object per line, which may arrive in parts.
    input.push(b"{\"Width\":80,\"Height\":24}\n{\"Width\":");
    assert_eq!(
        input.pop(),
        Some(Bytes::from_static(b"{\"Width\":80,\"Height\":24}"))
    );
    input.push(b"100,\"Height\":40}\n");
    assert_eq!(
        input.pop(),
        Some(Bytes::from_static(b"{\"Width\":100,\"Height\":40}"))
    );
    assert!(input.buf.is_empty());
    assert_eq!(input.pop(), None);

    // Only the latest of many sizes at once gets applied.
    input.push(&b"{\"Width\":1,\"Height\":2}".repeat(1000));
    input.push(b"{\"Width\":3,\"Height\":4}");
    assert_eq!(
        input.pop(),
        Some(Bytes::from_static(b"{\"Width\":3,\"Height\":4}"))
    );
    assert_eq!(input.pop(), None);

    // Garbage gets dropped, later events still work.
    input.push(b"}{\"Width\":1,\"Height\":2}");
    input.push(b"{\"Width\":1,\"Height\":2}");
    assert_eq!(
        input.pop(),
        Some(Bytes::from_static(b"{\"Width\":1,\"Height\":2}"))
    );

    // An object which never ends is bounded.
    input.push(&vec![b'['; MAX_RESIZE_BUFFER + 1]);
    assert!(input.buf.is_empty());
}

#[tokio::test]
async fn late_resize_streams_survive_a_cancelled_next() -> Result<()> {
    let (client, server) = duplex(1024);
    let mut connection =
        Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
    let mut client = Client::new(client)?;
    client.open(1, [("streamType", "error")].into()).await?;
    client.open(3, [("streamType", "stdout")].into()).await?;
    let streams = SpdyStreams::wait(
        &mut connection,
        ExpectedStreams {
            stdin: false,
            stdout: true,
            stderr: false,
        },
    )
    .await?;
    let (mut input, output) = streams.into_parts(connection, Version::V4).await;
    let mut output = Output::spdy(output);

    // The client does not read, the data queue of the connection fills.
    let data = Bytes::from(vec![0; 32 * 1024]);
    let fill = async { while output.data(STDOUT_BYTE, &data).await.is_ok() {} };
    assert!(
        time::timeout(Duration::from_millis(500), fill)
            .await
            .is_err()
    );

    // The resize stream comes meanwhile, the session cancels next() like
    // when its stdin channel gets a slot first.
    client.open(5, [("streamType", "resize")].into()).await?;
    assert!(
        time::timeout(Duration::from_millis(500), input.next())
            .await
            .is_err()
    );

    // The stream still works once the client reads again.
    client
        .send(Frame::Data {
            stream_id: 5,
            fin: false,
            data: Bytes::from_static(b"{\"Width\":1,\"Height\":2}\n"),
        })
        .await?;
    // The resets the client got, until the connection ended. A pause of the
    // frames, like on a slow machine, must not end the collection early.
    let resets = tokio::spawn(async move {
        let mut resets = vec![];
        while let Ok(Some(frame)) = client.frame().await {
            if let Frame::RstStream { stream_id, .. } = frame {
                resets.push(stream_id);
            }
        }
        resets
    });
    assert_eq!(
        time::timeout(Duration::from_secs(5), input.next()).await?,
        Some(ClientEvent::Resize(Bytes::from_static(
            b"{\"Width\":1,\"Height\":2}"
        )))
    );
    // Ends the connection once the frames which are queued went out.
    assert!(output.close().await);
    drop((input, output));
    let resets = time::timeout(Duration::from_secs(10), resets).await??;
    assert!(!resets.contains(&5), "{resets:?}");
    Ok(())
}

#[tokio::test]
async fn stdin_before_the_end_of_the_connection_gets_taken() -> Result<()> {
    for fin in [true, false] {
        let (client, server) = duplex(1024);
        let mut connection =
            Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
        let mut client = Client::new(client)?;
        client.open(1, [("streamType", "error")].into()).await?;
        client.open(3, [("streamType", "stdin")].into()).await?;
        let streams = SpdyStreams::wait(
            &mut connection,
            ExpectedStreams {
                stdin: true,
                stdout: false,
                stderr: false,
            },
        )
        .await?;
        let (mut input, _output) = streams.into_parts(connection, Version::V4).await;

        client
            .send(Frame::Data {
                stream_id: 3,
                fin,
                data: Bytes::from_static(b"data"),
            })
            .await?;
        drop(client);
        let done = input.done.clone();
        time::timeout(Duration::from_secs(5), done.cancelled()).await?;

        let data = Bytes::from_static(b"data");
        assert_eq!(
            input.next().await,
            Some(ClientEvent::Stdin(Stdin::Data(data)))
        );
        // Only after the end of stdin, the end of the connection is a
        // clean close.
        if fin {
            assert_eq!(input.next().await, Some(ClientEvent::Stdin(Stdin::Close)));
            assert_eq!(input.next().await, Some(ClientEvent::Closed));
        } else {
            assert_eq!(input.next().await, None);
        }
    }
    Ok(())
}

#[tokio::test]
async fn connection_end_without_stdin_is_no_clean_close() -> Result<()> {
    let (client, server) = duplex(1024);
    let mut connection =
        Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
    let mut client = Client::new(client)?;
    client.open(1, [("streamType", "error")].into()).await?;
    client.open(3, [("streamType", "stdout")].into()).await?;
    let streams = SpdyStreams::wait(
        &mut connection,
        ExpectedStreams {
            stdin: false,
            stdout: true,
            stderr: false,
        },
    )
    .await?;
    let (mut input, output) = streams.into_parts(connection, Version::V4).await;
    drop(client);
    assert_eq!(
        time::timeout(Duration::from_secs(5), input.next()).await?,
        None
    );
    assert!(!output.peer_closed.load(Ordering::Acquire));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn reset_output_after_the_stdin_close_fails_the_session() -> Result<()> {
    let (client, server) = duplex(64 * 1024);
    let mut connection =
        Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
    let mut client = Client::new(client)?;
    client.open(1, [("streamType", "error")].into()).await?;
    client.open(3, [("streamType", "stdout")].into()).await?;
    let streams = SpdyStreams::wait(
        &mut connection,
        ExpectedStreams {
            stdin: false,
            stdout: true,
            stderr: false,
        },
    )
    .await?;
    let (_input, output) = streams.into_parts(connection, Version::V4).await;
    let mut output = Output::spdy(output);
    // Like after the stdin close of the client.
    output.peer_closed().store(true, Ordering::Release);

    // The client resets stdout while the connection is still up.
    client
        .send(Frame::RstStream {
            stream_id: 3,
            // CANCEL
            status: 5,
        })
        .await?;
    let data = Bytes::from_static(b"out");
    time::timeout(Duration::from_secs(5), async {
        while output.data(STDOUT_BYTE, &data).await.is_ok() {
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    drop(client);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn closing_a_session_of_a_client_which_does_not_read_ends() -> Result<()> {
    let (client, server) = duplex(1024);
    let mut connection =
        Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
    let mut client = Client::new(client)?;
    client.open(1, [("streamType", "error")].into()).await?;
    client.open(3, [("streamType", "stdout")].into()).await?;
    let streams = SpdyStreams::wait(
        &mut connection,
        ExpectedStreams {
            stdin: false,
            stdout: true,
            stderr: false,
        },
    )
    .await?;
    let (_input, output) = streams.into_parts(connection, Version::V4).await;
    let mut output = Output::spdy(output);

    // The client does not read, the output backs up.
    let data = Bytes::from(vec![0; 32 * 1024]);
    let backed_up = async { while output.data(STDOUT_BYTE, &data).await.is_ok() {} };
    assert!(
        time::timeout(Duration::from_secs(1), backed_up)
            .await
            .is_err()
    );

    // The end of the session does not wait for the client forever, the
    // status and the close share the timeout.
    let start = time::Instant::now();
    assert!(output.status(&Status::Success).await.is_err());
    assert!(!output.close().await);
    assert_eq!(start.elapsed(), STALL_TIMEOUT);
    drop(client);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn slow_clients_get_all_output_and_the_status() -> Result<()> {
    let (client, server) = duplex(64 * 1024);
    let mut connection =
        Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
    let mut client = Client::new(client)?;
    client.open(1, [("streamType", "error")].into()).await?;
    client.open(3, [("streamType", "stdout")].into()).await?;
    let streams = SpdyStreams::wait(
        &mut connection,
        ExpectedStreams {
            stdin: false,
            stdout: true,
            stderr: false,
        },
    )
    .await?;
    let (_input, output) = streams.into_parts(connection, Version::V4).await;
    let mut output = Output::spdy(output);

    // A client which takes 16 KiB per second, which reads all output only
    // long after the timeouts, but never stops.
    let reader = tokio::spawn(async move {
        let mut read = vec![];
        let mut buf = vec![0; 16 * 1024];
        loop {
            match client.io().read(&mut buf).await {
                Ok(0) | Err(_) => return read,
                Ok(n) => read.extend_from_slice(&buf[..n]),
            }
            time::sleep(Duration::from_secs(1)).await;
        }
    });

    // The process writes 1 MiB and exits.
    let data = Bytes::from(vec![7; 8 * 1024]);
    for _ in 0..128 {
        output.data(STDOUT_BYTE, &data).await?;
    }
    let start = time::Instant::now();
    output.status(&Status::Success).await?;
    assert!(output.close().await);
    assert!(start.elapsed() > 2 * STALL_TIMEOUT);
    drop(output);

    let frames = spdy::testing::frames(&reader.await?).await?;
    let stdout: usize = frames
        .iter()
        .map(|frame| match frame {
            Frame::Data {
                stream_id: 3, data, ..
            } => data.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(stdout, 128 * data.len());
    let status = frames.iter().find_map(|frame| match frame {
        Frame::Data {
            stream_id: 1, data, ..
        } if !data.is_empty() => Some(data.clone()),
        _ => None,
    });
    assert_eq!(status.as_deref(), Some(SUCCESS_STATUS));
    // Both streams got closed.
    for id in [1, 3] {
        assert!(
            frames.iter().any(|frame| matches!(
                frame,
                Frame::Data { stream_id, fin: true, .. } if *stream_id == id
            )),
            "stream {id} not closed"
        );
    }
    Ok(())
}
