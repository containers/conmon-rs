use super::{
    frame::{GOAWAY_PROTOCOL_ERROR, STATUS_PROTOCOL_ERROR, STATUS_REFUSED_STREAM},
    testing::Client,
    *,
};
use tokio::io::duplex;

fn connect(
    accept: fn(&Headers) -> Result<()>,
    idle_timeout: Option<Duration>,
) -> Result<(Client, Connection)> {
    let (client, server) = Client::pair()?;
    Ok((
        client,
        Connection::serve(server, idle_timeout, None, accept),
    ))
}

fn accept_data(headers: &Headers) -> Result<()> {
    match headers.get("streamType") {
        Some("data") => Ok(()),
        typ => bail!("unexpected stream type {typ:?}"),
    }
}

#[tokio::test]
async fn streams_carry_data_both_ways() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client.open(1, [("streamtype", "data")].into()).await?;
    let mut stream = connection.accept().await.context("stream")?;
    assert_eq!(stream.id(), 1);
    assert_eq!(stream.headers().get("streamtype"), Some("data"));
    assert_eq!(
        client.frame().await?,
        Some(Frame::SynReply {
            stream_id: 1,
            headers: Headers::default()
        })
    );

    client
        .send(Frame::Data {
            stream_id: 1,
            fin: true,
            data: Bytes::from_static(b"ping"),
        })
        .await?;
    stream.send(Bytes::from_static(b"pong")).await?;
    stream.close().await?;

    let (mut receiver, sender) = stream.split();
    assert_eq!(receiver.recv().await, Some(Bytes::from_static(b"ping")));
    assert_eq!(receiver.recv().await, None);
    assert!(!receiver.is_reset());

    assert_eq!(
        client.frame().await?,
        Some(Frame::Data {
            stream_id: 1,
            fin: false,
            data: Bytes::from_static(b"pong")
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
    // Both sides are closed, the connection forgot the stream.
    assert!(sender.shared.lock_streams().is_empty());
    Ok(())
}

#[tokio::test]
async fn rejected_streams_get_reset() -> Result<()> {
    let (mut client, _connection) = connect(accept_data, None)?;
    client.open(1, [("streamtype", "unknown")].into()).await?;
    assert_eq!(
        client.frame().await?,
        Some(Frame::RstStream {
            stream_id: 1,
            status: STATUS_CANCEL
        })
    );
    Ok(())
}

#[tokio::test]
async fn invalid_stream_ids_get_reset() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client.open(3, [("streamtype", "data")].into()).await?;
    let _stream = connection.accept().await.context("stream")?;
    client.frame().await?;

    for stream_id in [2, 3, 1] {
        client
            .open(stream_id, [("streamtype", "data")].into())
            .await?;
        assert_eq!(
            client.frame().await?,
            Some(Frame::RstStream {
                stream_id,
                status: STATUS_PROTOCOL_ERROR
            })
        );
    }
    Ok(())
}

#[tokio::test]
async fn pings_get_echoed() -> Result<()> {
    let (mut client, _connection) = connect(accept_data, None)?;
    client.send(Frame::Ping { id: 2 }).await?;
    client.send(Frame::Ping { id: 1 }).await?;
    // Only the one the client started.
    assert_eq!(client.frame().await?, Some(Frame::Ping { id: 1 }));
    Ok(())
}

#[tokio::test]
async fn client_resets_end_the_stream() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client.open(1, [("streamtype", "data")].into()).await?;
    let stream = connection.accept().await.context("stream")?;
    client.frame().await?;

    client
        .send(Frame::RstStream {
            stream_id: 1,
            status: STATUS_CANCEL,
        })
        .await?;
    let (mut receiver, mut sender) = stream.split();
    time::timeout(Duration::from_secs(5), sender.reset_by_peer()).await?;
    assert_eq!(receiver.recv().await, None);
    assert!(receiver.is_reset());
    assert!(sender.send(Bytes::from_static(b"late")).await.is_err());
    Ok(())
}

#[tokio::test]
async fn dropped_senders_reset_the_stream() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client.open(1, [("streamtype", "data")].into()).await?;
    drop(connection.accept().await.context("stream")?);
    client.frame().await?;
    assert_eq!(
        client.frame().await?,
        Some(Frame::RstStream {
            stream_id: 1,
            status: STATUS_CANCEL
        })
    );
    Ok(())
}

#[tokio::test]
async fn closing_the_connection_ends_all_streams() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client.open(1, [("streamtype", "data")].into()).await?;
    let stream = connection.accept().await.context("stream")?;
    client.frame().await?;

    drop(client);
    let (mut receiver, mut sender) = stream.split();
    time::timeout(Duration::from_secs(5), sender.reset_by_peer()).await?;
    assert_eq!(receiver.recv().await, None);
    assert!(sender.send(Bytes::from_static(b"late")).await.is_err());
    assert!(connection.accept().await.is_none());
    Ok(())
}

#[tokio::test]
async fn protocol_errors_end_the_connection() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client.open(3, [("streamtype", "data")].into()).await?;
    let _stream = connection.accept().await.context("stream")?;
    client.frame().await?;
    // A control frame of SPDY/2.
    client
        .io()
        .write_all(&[0x80, 0x02, 0, 6, 0, 0, 0, 4, 0, 0, 0, 1])
        .await?;
    // The GOAWAY names the last stream of the client.
    assert_eq!(
        client.frame().await?,
        Some(Frame::GoAway {
            last_stream_id: 3,
            status: GOAWAY_PROTOCOL_ERROR
        })
    );
    assert_eq!(client.frame().await?, None);
    assert!(connection.accept().await.is_none());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn idle_connections_get_closed() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, Some(Duration::from_secs(60)))?;
    time::sleep(Duration::from_secs(50)).await;
    // Any frame keeps the connection open, also a ping.
    client.send(Frame::Ping { id: 1 }).await?;
    assert_eq!(client.frame().await?, Some(Frame::Ping { id: 1 }));
    time::sleep(Duration::from_secs(50)).await;
    assert!(
        time::timeout(Duration::from_secs(1), connection.accept())
            .await
            .is_err()
    );

    assert_eq!(
        client.frame().await?,
        Some(Frame::GoAway {
            last_stream_id: 0,
            status: GOAWAY_OK
        })
    );
    assert!(connection.accept().await.is_none());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn streams_which_take_data_slowly_keep_connections_active() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, Some(Duration::from_secs(30)))?;
    client.open(1, [("streamtype", "data")].into()).await?;
    let (mut receiver, _sender) = connection.accept().await.context("stream")?.split();
    client.frame().await?;
    let frames = STREAM_QUEUE + 2;
    for _ in 0..frames {
        client
            .send(Frame::Data {
                stream_id: 1,
                fin: false,
                data: Bytes::from_static(b"data"),
            })
            .await?;
    }

    // The client keeps waiting for the stream far longer than the idle
    // timeout, but the stream takes some of its data within each timeout.
    for _ in 0..10 {
        time::sleep(Duration::from_secs(20)).await;
        assert!(receiver.recv().await.is_some());
        client
            .send(Frame::Data {
                stream_id: 1,
                fin: false,
                data: Bytes::from_static(b"data"),
            })
            .await?;
    }
    // Still open, the connection answers once the stream took the rest.
    for _ in 0..frames {
        assert!(receiver.recv().await.is_some());
    }
    client.send(Frame::Ping { id: 1 }).await?;
    assert_eq!(client.frame().await?, Some(Frame::Ping { id: 1 }));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn idle_timeouts_end_connections_with_stuck_streams() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, Some(Duration::from_secs(30)))?;
    client.open(1, [("streamtype", "data")].into()).await?;
    // Accepted, but never read.
    let _stream = connection.accept().await.context("stream")?;
    client.frame().await?;
    for _ in 0..STREAM_QUEUE + 2 {
        client
            .send(Frame::Data {
                stream_id: 1,
                fin: false,
                data: Bytes::from_static(b"data"),
            })
            .await?;
    }

    assert_eq!(
        client.frame().await?,
        Some(Frame::GoAway {
            last_stream_id: 1,
            status: GOAWAY_OK
        })
    );
    assert!(connection.accept().await.is_none());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn huge_idle_timeouts_are_none() -> Result<()> {
    let (mut client, _connection) = connect(accept_data, Some(Duration::MAX))?;
    time::sleep(Duration::from_secs(3600)).await;
    client.send(Frame::Ping { id: 1 }).await?;
    assert_eq!(client.frame().await?, Some(Frame::Ping { id: 1 }));
    Ok(())
}

#[tokio::test]
#[expect(clippy::cast_possible_truncation, reason = "small constants")]
async fn resets_get_through_a_busy_connection() -> Result<()> {
    // A client which does not read: the writer waits for the connection
    // and the pings fill the control queue.
    let (client, server) = duplex(64);
    let mut connection = Connection::serve(server, None, None, accept_data);
    let mut client = Client::new(client)?;
    client.open(1, [("streamtype", "data")].into()).await?;
    let stream = connection.accept().await.context("stream")?;
    let pings = async {
        for id in (1..2 * CONTROL_QUEUE as u32).step_by(2) {
            client.send(Frame::Ping { id }).await?;
        }
        anyhow::Ok(())
    };
    let _ = time::timeout(Duration::from_millis(100), pings).await;
    drop(stream);

    // Once the client reads again, the reset arrives.
    let reset = async {
        loop {
            match client.frame().await? {
                Some(Frame::RstStream { stream_id: 1, .. }) => return anyhow::Ok(()),
                Some(_) => {}
                None => bail!("connection ended without the reset"),
            }
        }
    };
    time::timeout(Duration::from_secs(5), reset).await??;
    Ok(())
}

#[tokio::test]
async fn queued_data_goes_out_when_the_connection_ends() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client.open(1, [("streamtype", "data")].into()).await?;
    let stream = connection.accept().await.context("stream")?;
    client.frame().await?;

    // Queued without yielding to the writer, which then sees the end of
    // the connection first.
    let (_receiver, mut sender) = stream.split();
    sender.send(Bytes::from_static(b"last")).await?;
    sender.shared.done.cancel();

    assert_eq!(
        client.frame().await?,
        Some(Frame::Data {
            stream_id: 1,
            fin: false,
            data: Bytes::from_static(b"last")
        })
    );
    Ok(())
}

#[tokio::test]
async fn resets_follow_the_queued_data() -> Result<()> {
    for shutdown in [false, true] {
        let (mut client, mut connection) = connect(accept_data, None)?;
        client.open(1, [("streamtype", "data")].into()).await?;
        client.open(3, [("streamtype", "data")].into()).await?;
        let first = connection.accept().await.context("stream")?;
        let mut second = connection.accept().await.context("stream")?;
        client.frame().await?;
        client.frame().await?;

        // Queued without yielding to the writer.
        let (_receiver, mut sender) = first.split();
        sender.send(Bytes::from_static(b"first")).await?;
        second.send(Bytes::from_static(b"second")).await?;
        let shared = sender.shared.clone();
        sender.reset();
        if shutdown {
            shared.go_away(GOAWAY_OK);
            // Data of existing streams may follow the GOAWAY.
            assert!(matches!(client.frame().await?, Some(Frame::GoAway { .. })));
        }

        assert_eq!(
            client.frame().await?,
            Some(Frame::Data {
                stream_id: 1,
                fin: false,
                data: Bytes::from_static(b"first")
            })
        );
        assert!(matches!(
            client.frame().await?,
            Some(Frame::RstStream { stream_id: 1, .. })
        ));
        assert_eq!(
            client.frame().await?,
            Some(Frame::Data {
                stream_id: 3,
                fin: false,
                data: Bytes::from_static(b"second")
            })
        );
        // Nothing is left behind for the streams.
        assert_eq!(sender.queued.frames(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn cancelled_sends_do_not_lose_the_reset() -> Result<()> {
    for close in [false, true] {
        // A client which does not read, so that the data queue fills up.
        let (client, server) = duplex(64);
        let mut connection = Connection::serve(server, None, None, accept_data);
        let mut client = Client::new(client)?;
        client.open(1, [("streamtype", "data")].into()).await?;
        let (_receiver, mut sender) = connection.accept().await.context("stream")?.split();
        let shared = sender.shared.clone();

        // Until a send has to wait, which gets cancelled.
        while time::timeout(
            Duration::from_millis(50),
            sender.send(Bytes::from(vec![0; 1024])),
        )
        .await
        .is_ok()
        {}
        if close {
            assert!(
                time::timeout(Duration::from_millis(50), sender.close())
                    .await
                    .is_err()
            );
            // Not closed, so the drop resets it.
            drop(sender);
        } else {
            sender.reset();
        }

        // The reset follows the queued data once the client reads.
        let reset = async {
            loop {
                match client.frame().await? {
                    Some(Frame::RstStream { stream_id: 1, .. }) => return anyhow::Ok(()),
                    Some(_) => {}
                    None => bail!("connection ended without the reset"),
                }
            }
        };
        time::timeout(Duration::from_secs(5), reset).await??;
        assert!(shared.lock_streams().is_empty());
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_server_pings_its_clients() -> Result<()> {
    let (mut client, server) = Client::pair()?;
    let _connection = Connection::serve(
        server,
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(100)),
        accept_data,
    );
    let Some(Frame::Ping { id }) = client.frame().await? else {
        bail!("no ping");
    };
    assert_eq!(id % 2, 0);
    // Its own pings do not keep the connection from being idle.
    client.send(Frame::Ping { id }).await?;
    loop {
        match client.frame().await? {
            Some(Frame::Ping { id }) if id % 2 == 0 => {}
            frame => {
                assert!(matches!(frame, Some(Frame::GoAway { .. })), "{frame:?}");
                break;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn gone_clients_end_connections_with_stuck_streams() -> Result<()> {
    let (mut client, server) = Client::pair()?;
    let mut connection =
        Connection::serve(server, None, Some(Duration::from_millis(100)), accept_data);
    client.open(1, [("streamtype", "data")].into()).await?;
    // Accepted, but never read.
    let _stream = connection.accept().await.context("stream")?;
    client.frame().await?;
    for _ in 0..STREAM_QUEUE + 2 {
        client
            .send(Frame::Data {
                stream_id: 1,
                fin: false,
                data: Bytes::from_static(b"data"),
            })
            .await?;
    }

    // Nothing reads the end of the connection, the next ping fails.
    drop(client);
    let end = time::timeout(Duration::from_secs(5), connection.accept()).await?;
    assert!(end.is_none());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn closing_flushes_before_the_idle_timeout() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, Some(Duration::from_secs(60)))?;
    client.open(1, [("streamtype", "data")].into()).await?;
    let mut stream = connection.accept().await.context("stream")?;
    stream.send(Bytes::from_static(b"last")).await?;
    stream.close().await?;
    connection.handle().close();

    // Everything queued goes out, then the server closes the connection
    // instead of waiting for the idle timeout.
    let mut frames = vec![];
    while let Some(frame) = client.frame().await? {
        frames.push(frame);
    }
    assert_eq!(
        frames,
        [
            Frame::SynReply {
                stream_id: 1,
                headers: Headers::default()
            },
            Frame::Data {
                stream_id: 1,
                fin: false,
                data: Bytes::from_static(b"last")
            },
            Frame::Data {
                stream_id: 1,
                fin: true,
                data: Bytes::new()
            },
        ]
    );
    time::timeout(
        Duration::from_secs(1),
        connection.handle().done().cancelled(),
    )
    .await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn ended_connections_read_the_client_until_it_closed() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    connection.handle().close();
    assert_eq!(client.frame().await?, None);
    time::timeout(
        Duration::from_secs(1),
        connection.handle().done().cancelled(),
    )
    .await?;
    let accepted = time::timeout(Duration::from_secs(1), connection.accept()).await?;
    assert!(accepted.is_none());

    // Input the server does not read would reset the connection, which
    // could drop its last frames.
    time::sleep(Duration::from_millis(50)).await;
    let data = Bytes::from(vec![0; 60_000]);
    for _ in 0..40 {
        client
            .send(Frame::Data {
                stream_id: 1,
                fin: false,
                data: data.clone(),
            })
            .await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn closing_a_stuck_connection_ends_with_the_idle_timeout() -> Result<()> {
    // A client which does not read: the close cannot flush, the idle
    // watch still ends the connection.
    let (client, server) = duplex(64);
    let mut connection =
        Connection::serve(server, Some(Duration::from_secs(60)), None, accept_data);
    let mut client = Client::new(client)?;
    client.open(1, [("streamtype", "data")].into()).await?;
    let mut stream = connection.accept().await.context("stream")?;
    let send =
        tokio::spawn(async move { while stream.send(Bytes::from(vec![0; 1024])).await.is_ok() {} });
    time::sleep(Duration::from_secs(1)).await;
    connection.handle().close();
    assert!(!connection.handle().done().is_cancelled());

    time::timeout(
        Duration::from_secs(120),
        connection.handle().done().cancelled(),
    )
    .await?;
    time::timeout(Duration::from_secs(5), send).await??;
    assert!(connection.accept().await.is_none());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn streams_get_refused_without_the_connection() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client.open(1, [("streamtype", "data")].into()).await?;
    // Opened before, but nobody takes it.
    client.open(3, [("streamtype", "data")].into()).await?;
    let _stream = connection.accept().await.context("stream")?;
    time::sleep(Duration::from_millis(100)).await;
    drop(connection);
    client.open(5, [("streamtype", "data")].into()).await?;

    let mut resets = vec![];
    while resets.len() < 2 {
        if let Some(Frame::RstStream { stream_id, status }) = client.frame().await? {
            resets.push((stream_id, status));
        }
    }
    assert_eq!(resets, [(3, STATUS_CANCEL), (5, STATUS_REFUSED_STREAM)]);
    // The connection goes on.
    client.send(Frame::Ping { id: 1 }).await?;
    assert_eq!(client.frame().await?, Some(Frame::Ping { id: 1 }));
    Ok(())
}

#[tokio::test]
#[expect(clippy::cast_possible_truncation, reason = "a wrapping byte pattern")]
async fn slow_streams_do_not_lose_data() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client.open(1, [("streamtype", "data")].into()).await?;
    let stream = connection.accept().await.context("stream")?;
    client.frame().await?;

    // More frames than the stream queue holds, which the reader waits
    // with.
    let frames = STREAM_QUEUE * 4;
    for i in 0..frames {
        client
            .send(Frame::Data {
                stream_id: 1,
                fin: i + 1 == frames,
                data: Bytes::from(vec![i as u8; 1024]),
            })
            .await?;
    }
    let (mut receiver, _sender) = stream.split();
    let mut received = 0;
    while let Some(data) = receiver.recv().await {
        assert_eq!(data, vec![received as u8; 1024]);
        received += 1;
    }
    assert_eq!(received, frames);
    Ok(())
}

#[tokio::test]
#[expect(clippy::cast_possible_truncation, reason = "small constants")]
async fn too_many_streams_get_refused() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    let mut streams = Vec::with_capacity(MAX_STREAMS);
    for i in 0..MAX_STREAMS as u32 {
        client
            .open(2 * i + 1, [("streamtype", "data")].into())
            .await?;
        streams.push(connection.accept().await.context("stream")?);
    }
    let last = 2 * MAX_STREAMS as u32 + 1;
    client.open(last, [("streamtype", "data")].into()).await?;
    loop {
        match client.frame().await?.context("frame")? {
            Frame::SynReply { stream_id, .. } if stream_id < last => {}
            frame => {
                assert_eq!(
                    frame,
                    Frame::RstStream {
                        stream_id: last,
                        status: STATUS_REFUSED_STREAM
                    }
                );
                break;
            }
        }
    }

    // A stream which ended makes room for the next one.
    drop(streams.pop());
    assert!(matches!(
        client.frame().await?,
        Some(Frame::RstStream { .. })
    ));
    client
        .open(last + 2, [("streamtype", "data")].into())
        .await?;
    let stream = connection.accept().await.context("stream")?;
    assert_eq!(stream.id(), last + 2);
    Ok(())
}

#[tokio::test]
#[expect(clippy::cast_possible_truncation, reason = "small constants")]
async fn streams_reset_by_the_client_count_until_dropped() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    let mut streams = Vec::with_capacity(MAX_STREAMS);
    for i in 0..MAX_STREAMS as u32 {
        let stream_id = 2 * i + 1;
        client
            .open(stream_id, [("streamtype", "data")].into())
            .await?;
        client
            .send(Frame::RstStream {
                stream_id,
                status: STATUS_CANCEL,
            })
            .await?;
        streams.push(connection.accept().await.context("stream")?);
    }
    // The client reset all of them, the server still holds them.
    let last = 2 * MAX_STREAMS as u32 + 1;
    client.open(last, [("streamtype", "data")].into()).await?;
    client
        .frame_until(|f| {
            matches!(
                f,
                Frame::RstStream {
                    stream_id,
                    status: STATUS_REFUSED_STREAM
                } if *stream_id == last
            )
        })
        .await?;
    assert!(streams.iter().all(Stream::is_reset));

    // Once the server let go of one, the next one gets accepted.
    streams.pop();
    client
        .open(last + 2, [("streamtype", "data")].into())
        .await?;
    let stream = connection.accept().await.context("stream")?;
    assert_eq!(stream.id(), last + 2);
    drop(streams);
    assert_eq!(stream.sender.shared.live_streams.load(Ordering::Acquire), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
#[expect(clippy::cast_possible_truncation, reason = "small constants")]
async fn streams_share_a_byte_budget() -> Result<()> {
    const FRAME_LEN: usize = 64 * 1024;
    // As many streams as the budget holds with full queues, and one more.
    let full = DATA_BUFFER as usize / (STREAM_QUEUE * FRAME_LEN);
    assert!(full > 1, "a single stream must not take all of the budget");

    let (mut client, mut connection) = connect(accept_data, None)?;
    let mut receivers = vec![];
    for i in 0..=full as u32 {
        client
            .open(2 * i + 1, [("streamtype", "data")].into())
            .await?;
        let (receiver, sender) = connection.accept().await.context("stream")?.split();
        receivers.push((receiver, sender));
        client.frame().await?;
    }
    let data = Bytes::from(vec![0; FRAME_LEN]);
    for i in 0..full as u32 {
        for _ in 0..STREAM_QUEUE {
            client
                .send(Frame::Data {
                    stream_id: 2 * i + 1,
                    fin: false,
                    data: data.clone(),
                })
                .await?;
        }
    }
    // The queue of the last stream has room, the budget does not.
    client
        .send(Frame::Data {
            stream_id: 2 * full as u32 + 1,
            fin: false,
            data: data.clone(),
        })
        .await?;
    client.send(Frame::Ping { id: 1 }).await?;
    assert!(
        time::timeout(Duration::from_millis(200), client.frame())
            .await
            .is_err(),
        "the connection got read on"
    );
    assert_eq!(receivers[0].1.shared.budget.available(), 0);

    // A stream which went away while its data waited for the budget does
    // not hold up the connection.
    let (last, _last_sender) = receivers.pop().context("last stream")?;
    drop(last);
    assert_eq!(client.frame().await?, Some(Frame::Ping { id: 1 }));
    assert_eq!(receivers[0].1.shared.budget.available(), 0);

    // Data which got taken gives its share back.
    client
        .send(Frame::Data {
            stream_id: 1,
            fin: false,
            data: data.clone(),
        })
        .await?;
    client.send(Frame::Ping { id: 3 }).await?;
    assert_eq!(
        receivers[0].0.recv().await.map(|d| d.len()),
        Some(FRAME_LEN)
    );
    assert_eq!(client.frame().await?, Some(Frame::Ping { id: 3 }));

    // Dropped data does as well.
    drop(receivers);
    time::sleep(Duration::from_millis(100)).await;
    assert_eq!(connection.shared.budget.available(), DATA_BUFFER as usize);
    Ok(())
}

#[tokio::test]
async fn streams_transfer_more_than_the_budget() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client.open(1, [("streamtype", "data")].into()).await?;
    let (mut receiver, _sender) = connection.accept().await.context("stream")?.split();
    let received = tokio::spawn(async move {
        let mut len = 0;
        while let Some(data) = receiver.recv().await {
            len += data.len();
        }
        len
    });

    let data = Bytes::from(vec![0; 32 * 1024]);
    let frames = 4 * DATA_BUFFER as usize / data.len();
    for i in 0..frames {
        client
            .send(Frame::Data {
                stream_id: 1,
                fin: i + 1 == frames,
                data: data.clone(),
            })
            .await?;
    }
    assert_eq!(received.await?, 4 * DATA_BUFFER as usize);
    Ok(())
}

#[tokio::test]
async fn streams_opened_with_fin_have_no_data() -> Result<()> {
    let (mut client, mut connection) = connect(accept_data, None)?;
    client
        .send(Frame::SynStream {
            stream_id: 1,
            fin: true,
            headers: [("streamtype", "data")].into(),
        })
        .await?;
    let (mut receiver, _sender) = connection.accept().await.context("stream")?.split();
    assert_eq!(receiver.recv().await, None);
    assert!(!receiver.is_reset());
    Ok(())
}
