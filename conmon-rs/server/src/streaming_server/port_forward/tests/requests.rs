//! The SPDY protocol: the streams of a request, and their errors.

use super::*;

#[tokio::test]
async fn forwards_a_connection() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let mut client = serve()?;
    request(&mut client, port).await?;

    client
        .send(Frame::Data {
            stream_id: 3,
            fin: true,
            data: Bytes::from_static(b"request"),
        })
        .await?;
    let (mut connection, _) = listener.accept().await?;
    let mut request = vec![];
    // The client closed its side, which ends the input of the pod.
    connection.read_to_end(&mut request).await?;
    assert_eq!(request, b"request");
    connection.write_all(b"response").await?;
    drop(connection);

    assert_eq!(
        client.frame().await?,
        Some(Frame::Data {
            stream_id: 3,
            fin: false,
            data: Bytes::from_static(b"response")
        })
    );
    // Both streams get closed, without an error.
    let mut closed = vec![];
    for _ in 0..2 {
        match client.frame().await? {
            Some(Frame::Data {
                stream_id,
                fin: true,
                data,
            }) if data.is_empty() => closed.push(stream_id),
            frame => panic!("unexpected frame {frame:?}"),
        }
    }
    closed.sort();
    assert_eq!(closed, [1, 3]);
    Ok(())
}

/// A pod which neither answers nor closes its side after the client closed
/// its one does not keep the streams of the request.
#[tokio::test(start_paused = true)]
async fn half_closed_connections_end_after_the_grace_period() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let mut client = serve()?;
    request(&mut client, port).await?;

    client
        .send(Frame::Data {
            stream_id: 3,
            fin: true,
            data: Bytes::from_static(b"request"),
        })
        .await?;
    let (mut connection, _) = listener.accept().await?;
    let mut request = vec![];
    connection.read_to_end(&mut request).await?;
    assert_eq!(request, b"request");

    // The pod keeps its side open. The clock is paused, so the wait for
    // the frames takes as long as the grace period, less than the timeout
    // of reading a frame.
    let start = time::Instant::now();
    let (mut reset, mut closed) = (false, false);
    while !(reset && closed) {
        match client.frame().await?.context("connection ended")? {
            Frame::RstStream { stream_id: 3, .. } => reset = true,
            // No error gets reported, like for a client which keeps its
            // side open.
            Frame::Data {
                stream_id: 1,
                fin: true,
                data,
            } if data.is_empty() => closed = true,
            frame => panic!("unexpected frame {frame:?}"),
        }
    }
    // The grace period which docs/protocols.md names.
    assert!(start.elapsed() >= Duration::from_secs(30));

    // The connection to the pod is gone as well.
    let mut buf = [0; 1];
    let gone = async {
        while connection.write(&buf).await.is_ok() {
            task::yield_now().await;
        }
    };
    time::timeout(Duration::from_secs(5), gone)
        .await
        .context("connection to the pod still open")?;
    assert_eq!(connection.read(&mut buf).await.unwrap_or_default(), 0);
    Ok(())
}

#[tokio::test]
async fn data_on_the_error_stream_gets_dropped() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let mut client = serve()?;
    client.open(1, headers(STREAM_TYPE_ERROR, port)).await?;
    client.open(3, headers(STREAM_TYPE_DATA, port)).await?;
    let (mut connection, _) = time::timeout(Duration::from_secs(5), listener.accept()).await??;

    // More than the connection holds, which nothing takes.
    let request = async {
        for _ in 0..80 {
            client
                .send(Frame::Data {
                    stream_id: 1,
                    fin: false,
                    data: Bytes::from(vec![0; 64 * 1024]),
                })
                .await?;
        }
        client
            .send(Frame::Data {
                stream_id: 3,
                fin: true,
                data: Bytes::from_static(b"request"),
            })
            .await?;
        let mut request = vec![];
        connection.read_to_end(&mut request).await?;
        anyhow::Ok(request)
    };
    let request = time::timeout(Duration::from_secs(5), request)
        .await
        .context("the connection is held up")??;
    assert_eq!(request, b"request");
    Ok(())
}

#[tokio::test]
async fn failures_get_reported_on_the_error_stream() -> Result<()> {
    let (_socket, port) = closed_port()?;
    let mut client = serve()?;
    request(&mut client, port).await?;

    let mut error = String::new();
    let mut reset = false;
    let mut closed = false;
    while !(reset && closed) {
        match client.frame().await?.context("connection ended")? {
            Frame::RstStream {
                stream_id: 3,
                status: _,
            } => reset = true,
            Frame::Data {
                stream_id: 1,
                fin,
                data,
            } => {
                error.push_str(&String::from_utf8_lossy(&data));
                closed = fin;
            }
            frame => panic!("unexpected frame {frame:?}"),
        }
    }
    assert!(
        error.starts_with(&format!(
            "error forwarding port {port}: failed to connect to localhost:{port}"
        )),
        "{error}"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn incomplete_requests_time_out() -> Result<()> {
    let mut client = serve()?;
    client.open(1, headers(STREAM_TYPE_ERROR, 80)).await?;
    client.frame().await?;

    assert_eq!(
        client.frame().await?,
        Some(Frame::Data {
            stream_id: 1,
            fin: false,
            data: Bytes::from_static(b"request 0: timed out waiting for streams")
        })
    );
    assert_eq!(
        client.frame().await?,
        Some(Frame::Data {
            stream_id: 1,
            fin: true,
            data: Bytes::new()
        })
    );
    Ok(())
}

#[tokio::test]
async fn invalid_streams_get_rejected() -> Result<()> {
    let mut client = serve()?;
    for (stream_id, headers) in [
        (
            1,
            Headers::from([("streamType", "data"), ("requestID", "0")]),
        ),
        (3, [("streamType", "data"), ("port", "0")].into()),
        (5, [("streamType", "data"), ("port", "http")].into()),
        (7, [("port", "80")].into()),
        (9, [("streamType", "resize"), ("port", "80")].into()),
    ] {
        client.open(stream_id, headers).await?;
        assert!(
            matches!(client.frame().await?, Some(Frame::RstStream { stream_id: id, .. }) if id == stream_id)
        );
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn reset_streams_leave_pending_requests() -> Result<()> {
    let mut client = serve()?;
    // Requests which wait for their second stream, as many as fit.
    let headers = |typ, request_id: &str| {
        Headers::from([
            ("streamType", typ),
            ("port", "80"),
            ("requestID", request_id),
        ])
    };
    let mut stream_id = 1;
    for i in 0..MAX_STREAMS {
        let typ = [STREAM_TYPE_ERROR, STREAM_TYPE_DATA][i % 2];
        client.open(stream_id, headers(typ, &i.to_string())).await?;
        stream_id += 2;
    }
    for stream_id in (1..stream_id).step_by(2) {
        client
            .send(Frame::RstStream {
                stream_id,
                status: 5,
            })
            .await?;
    }

    // The next stream gets room right away, not with the deadline of
    // the requests. It may come before the resets got handled.
    let opened = async {
        loop {
            client
                .open(stream_id, headers(STREAM_TYPE_ERROR, "next"))
                .await?;
            let frame = client
                .frame_until(|frame| {
                    matches!(
                        frame,
                        Frame::SynReply { stream_id: id, .. }
                            | Frame::RstStream { stream_id: id, .. } if *id == stream_id
                    )
                })
                .await?;
            if matches!(frame, Frame::SynReply { .. }) {
                return anyhow::Ok(());
            }
            stream_id += 2;
            time::sleep(Duration::from_millis(10)).await;
        }
    };
    time::timeout(Duration::from_secs(5), opened)
        .await
        .context("reset streams still pending")??;
    Ok(())
}

#[tokio::test]
async fn pending_requests_let_go_of_reset_streams() -> Result<()> {
    let (mut client, server) = Client::pair()?;
    let mut connection = Connection::serve(server, None, None, PortForward::accept);
    let namespace = Namespace::new(NETNS.into());
    let mut pending = Pending::new();
    // Requests a and c, whose only streams the client resets, and b.
    for (stream_id, typ, request_id) in [
        (1, STREAM_TYPE_ERROR, "a"),
        (3, STREAM_TYPE_ERROR, "b"),
        (5, STREAM_TYPE_DATA, "c"),
    ] {
        client
            .open(
                stream_id,
                [
                    ("streamType", typ),
                    ("port", "80"),
                    ("requestID", request_id),
                ]
                .into(),
            )
            .await?;
        let stream = connection.accept().await.context("stream")?;
        PortForward::add(&mut pending, stream, &namespace);
    }
    assert_eq!(pending.resets.len(), 3);
    for stream_id in [1, 5] {
        client
            .send(Frame::RstStream {
                stream_id,
                status: 5,
            })
            .await?;
    }
    for _ in 0..2 {
        let reset = pending.resets.next().await.context("watch")?;
        pending.remove_reset(&reset.context("reset")?);
    }
    assert_eq!(pending.requests.len(), 1);
    assert!(pending.requests.contains_key("b"));
    assert_eq!(pending.deadlines.len(), 1);

    // A request which got both streams stops watching.
    client
        .open(
            7,
            [("streamType", "data"), ("port", "80"), ("requestID", "b")].into(),
        )
        .await?;
    let stream = connection.accept().await.context("stream")?;
    PortForward::add(&mut pending, stream, &namespace);
    assert!(pending.requests.is_empty());
    assert_eq!(pending.resets.next().await, Some(None));
    assert!(pending.resets.is_empty());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn removed_requests_free_their_deadline() {
    let mut pending = Pending::new();
    let (a, b): (Arc<str>, Arc<str>) = ("a".into(), "b".into());
    pending.get_or_insert(&a);
    pending.get_or_insert(&a);
    pending.get_or_insert(&b);
    assert_eq!(pending.deadlines.len(), 2);
    assert!(pending.remove("a").is_some());
    assert!(pending.remove("a").is_none());
    assert_eq!(pending.deadlines.len(), 1);

    let expired = time::timeout(Duration::from_secs(60), pending.deadlines.next())
        .await
        .expect("expired")
        .expect("deadline");
    assert_eq!(expired.into_inner(), b);
    assert!(pending.deadlines.is_empty());
}

#[tokio::test]
async fn requests_without_ids_pair_by_stream_id() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port().to_string();
    let mut client = serve()?;
    // The data stream of the request of error stream 1 is stream 3.
    client
        .open(1, [("streamType", "error"), ("port", &port)].into())
        .await?;
    client
        .open(3, [("streamType", "data"), ("port", &port)].into())
        .await?;
    time::timeout(Duration::from_secs(5), listener.accept()).await??;
    Ok(())
}
