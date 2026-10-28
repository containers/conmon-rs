use super::{channels::ChannelOutput, requests::Pending, *};
use crate::{
    streaming_server::{
        spdy::{
            Connection, Headers, MAX_STREAMS,
            testing::{Client, Frame},
        },
        web::{self, Message},
    },
    test_util::{closed_port, wait_for},
};
use bytes::{BufMut, BytesMut};
use futures::{SinkExt, StreamExt};
use std::{
    net::Ipv4Addr,
    sync::{Arc, atomic::Ordering},
};
use tokio::{io::AsyncReadExt, net::TcpListener, sync::oneshot, task::JoinSet};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_util::sync::CancellationToken;

/// The network namespace of the tests, which needs no privileges to
/// connect in.
const NETNS: &str = "/proc/self/ns/net";

fn serve() -> Result<Client> {
    let (client, server) = Client::pair()?;
    tokio::spawn(
        PortForward::new(NETNS.into(), None)
            .without_pings()
            .serve_spdy(server),
    );
    Ok(client)
}

fn headers(typ: &str, port: u16) -> Headers {
    let port = port.to_string();
    [("streamType", typ), ("port", &port), ("requestID", "0")].into()
}

/// Open the streams of a request, the error stream closed right away
/// like clients do.
async fn request(client: &mut Client, port: u16) -> Result<()> {
    client.open(1, headers(STREAM_TYPE_ERROR, port)).await?;
    client
        .send(Frame::Data {
            stream_id: 1,
            fin: true,
            data: Bytes::new(),
        })
        .await?;
    client.open(3, headers(STREAM_TYPE_DATA, port)).await?;
    for stream_id in [1, 3] {
        assert_eq!(
            client.frame().await?,
            Some(Frame::SynReply {
                stream_id,
                headers: Headers::default()
            })
        );
    }
    Ok(())
}

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
async fn failed_channels_let_go_of_their_input() -> Result<()> {
    let (_socket, port) = closed_port()?;
    let budget = Budget::new(CHANNEL_BUFFER);
    let (input_tx, input) = mpsc::channel(CHANNEL_QUEUE);
    // A client which does not read, the error waits.
    let (messages, mut output) = mpsc::channel(1);
    messages.send(Message::Binary(Bytes::new())).await?;
    let forward = tokio::spawn(PortForward::forward_channel(
        Namespace::new(NETNS.into()),
        port,
        input,
        ChannelOutput {
            messages,
            base64: false,
            data: 0,
            error: 1,
        },
    ));

    // The input does not wait for the error, which still comes.
    let sent = async {
        for _ in 0..2 * CHANNEL_QUEUE {
            let data = budget.hold(Bytes::from(vec![0; READ_BUFFER])).await;
            if input_tx.send(data).await.is_err() {
                break;
            }
        }
    };
    time::timeout(Duration::from_secs(5), sent)
        .await
        .context("input still taken")?;
    time::timeout(Duration::from_secs(5), input_tx.closed())
        .await
        .context("input still taken")?;
    assert_eq!(budget.available(), CHANNEL_BUFFER as usize);
    output.recv().await;
    let error = output.recv().await.context("no error")?;
    assert!(
        matches!(&error, Message::Binary(data) if data.starts_with(b"\x01error forwarding port")),
        "{error:?}"
    );
    forward.await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn tunnels_end_with_clients_which_do_not_read() -> Result<()> {
    let (server, client) = tokio::io::duplex(64 * 1024);
    let (socket, mut client) = tokio::join!(
        WebSocketStream::from_raw_socket(web::ClientIo::new(server), Role::Server, None),
        WebSocketStream::from_raw_socket(client, Role::Client, None),
    );
    let (mut spdy, connection) = tokio::io::duplex(TUNNEL_BUFFER);
    let tunnel = tokio::spawn(PortForward::tunnel(socket, connection));

    // The SPDY connection sends more than the client reads, then ends.
    let output = vec![0; 8 * TUNNEL_BUFFER];
    assert!(
        time::timeout(Duration::from_secs(1), spdy.write_all(&output))
            .await
            .is_err()
    );
    drop(spdy);

    // The client keeps sending, which the tunnel cannot write anymore.
    let send = tokio::spawn(async move {
        while client
            .send(Message::Binary(Bytes::from_static(b"data")))
            .await
            .is_ok()
        {
            time::sleep(Duration::from_millis(100)).await;
        }
    });
    time::timeout(Duration::from_secs(30), tunnel)
        .await
        .context("the tunnel hangs")??;
    send.abort();
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

#[test]
fn websocket_protocols_get_negotiated() {
    let negotiate = |requested: &[&str]| WebSocketProtocol::negotiate(requested.iter().copied());
    assert_eq!(
        negotiate(&[]),
        Some((WebSocketProtocol::Channels { base64: false }, None))
    );
    assert_eq!(
        negotiate(&["v4.base64.channel.k8s.io", "v4.channel.k8s.io"]),
        Some((
            WebSocketProtocol::Channels { base64: true },
            Some(BASE64_CHANNEL_PROTOCOL)
        ))
    );
    // The tunnel wins, wherever the client lists it.
    assert_eq!(
        negotiate(&["v4.channel.k8s.io", "SPDY/3.1+portforward.k8s.io"]),
        Some((WebSocketProtocol::SpdyTunnel, Some(SPDY_TUNNEL_PROTOCOL)))
    );
    assert_eq!(negotiate(&["v5.channel.k8s.io", "channel.k8s.io"]), None);
}

#[test]
fn query_ports_get_parsed() -> Result<()> {
    assert_eq!(PortForward::query_ports(Some("port=80"))?, [80]);
    assert_eq!(
        PortForward::query_ports(Some("other=1&port=80&port=81%2C82,83"))?,
        [80, 81, 82, 83]
    );
    for query in [
        None,
        Some(""),
        Some("ports=80"),
        Some("port="),
        Some("port=0"),
        Some("port=http"),
        Some("port=65536"),
        Some("port=80,"),
    ] {
        assert!(PortForward::query_ports(query).is_err(), "{query:?}");
    }
    let too_many = vec!["port=1"; MAX_CHANNEL_PORTS + 1].join("&");
    assert!(PortForward::query_ports(Some(&too_many)).is_err());
    Ok(())
}

#[tokio::test]
async fn input_gets_taken_while_connecting() -> Result<()> {
    let (input_tx, mut input) = mpsc::channel(1);
    let (connected_tx, connected) = oneshot::channel();

    // More messages than the queue holds arrive before the connection.
    let send = tokio::spawn(async move {
        for i in 0..10u8 {
            input_tx.send(Bytes::from(vec![i]).into()).await?;
        }
        drop(input_tx);
        connected_tx
            .send(())
            .map_err(|()| anyhow::anyhow!("connect gone"))
    });
    let (connection, early) = PortForward::drain_until(
        async { connected.await.context("connected").map(|()| 42) },
        &Budget::new(CONNECT_BUDGET),
        &mut input,
    )
    .await?;
    send.await??;
    assert_eq!(connection, 42);
    // Most of it got taken, the rest is still queued for the copy.
    let mut received = early.data.to_vec();
    received.extend(early.last.iter().flat_map(|data| data.iter()));
    assert!(received.len() >= 9, "{early:?}");
    if !early.ended {
        while let Some(data) = input.recv().await {
            received.extend_from_slice(&data);
        }
    }
    assert_eq!(received, (0..10u8).collect::<Vec<_>>());
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a wrapping byte pattern"
)]
async fn small_input_while_connecting_gets_collected() -> Result<()> {
    let connect_budget = Budget::new(CONNECT_BUDGET);
    let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
    let (connected_tx, connected) = oneshot::channel();
    let send = tokio::spawn({
        let budget = Budget::new(1000);
        async move {
            for i in 0..1000 {
                let data = budget.hold(Bytes::from(vec![i as u8])).await;
                input_tx.send(data).await?;
            }
            // Connected once all of it moved into the connect budget.
            while budget.available() != 1000 {
                time::sleep(Duration::from_millis(10)).await;
            }
            connected_tx
                .send(())
                .map_err(|()| anyhow::anyhow!("connect gone"))
        }
    });
    let ((), early) = PortForward::drain_until(
        async { connected.await.context("connected") },
        &connect_budget,
        &mut input,
    )
    .await?;
    time::timeout(Duration::from_secs(5), send).await???;
    assert_eq!(
        &early.data[..],
        (0..1000).map(|i| i as u8).collect::<Vec<_>>()
    );
    assert!(early.last.is_none());
    // The buffer counts as a whole.
    assert!(connect_budget.available() <= CONNECT_BUDGET as usize - 1000);
    drop(early);
    assert_eq!(connect_budget.available(), CONNECT_BUDGET as usize);
    Ok(())
}

#[tokio::test]
async fn written_input_gives_its_budget_back() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let connection = TcpStream::connect(listener.local_addr()?).await?;
    let (mut pod, _) = listener.accept().await?;
    let (connect_budget, client_budget) = (Budget::new(10), Budget::new(10));
    let mut data = HeldBuf::default();
    data.push(connect_budget.hold(Bytes::from_static(b"ab")).await);
    let early = Early {
        data,
        last: Some(client_budget.hold(Bytes::from_static(b"c")).await),
        ended: false,
    };

    // The client keeps its input open.
    let (_input_tx, mut input) = mpsc::channel(1);
    let (_, mut to_container) = connection.into_split();
    let write = PortForward::write_input(early, &mut input, &mut to_container);
    let given_back = async {
        let mut buf = [0; 3];
        pod.read_exact(&mut buf).await?;
        assert_eq!(&buf, b"abc");
        while connect_budget.available() != 10 || client_budget.available() != 10 {
            time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    };
    time::timeout(Duration::from_secs(5), async {
        select! {
            result = write => bail!("input ended: {result:?}"),
            result = given_back => result,
        }
    })
    .await
    .context("budget still held")?
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

#[tokio::test]
async fn channel_input_is_bounded_by_bytes() -> Result<()> {
    let budget = Budget::new(CHANNEL_BUFFER);
    let idle = CancellationToken::new();
    let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
    let inputs = [input_tx];
    let message = |len| {
        let mut message = BytesMut::with_capacity(1 + len);
        message.put_u8(0);
        message.put_bytes(1, len);
        message.freeze()
    };

    // Fewer messages than the queue holds, but all of the budget.
    let len = CHANNEL_BUFFER as usize / 4;
    for _ in 0..4 {
        PortForward::dispatch(&inputs, &budget, false, message(len), &idle).await;
    }
    assert_eq!(budget.available(), 0);
    let next = PortForward::dispatch(&inputs, &budget, false, message(1), &idle);
    let mut next = pin!(next);
    assert!(
        time::timeout(Duration::from_millis(100), &mut next)
            .await
            .is_err()
    );

    // A message which got written to the pod makes room.
    assert_eq!(input.recv().await.map(|data| data.len()), Some(len));
    time::timeout(Duration::from_secs(1), next).await?;
    Ok(())
}

#[tokio::test]
async fn input_while_connecting_leaves_the_budget() -> Result<()> {
    // Connections to a port which does not answer take what the client
    // sends, which must not stop the other connections of the client.
    let budget = Budget::new(CHANNEL_BUFFER);
    let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
    let connecting = tokio::spawn(async move {
        PortForward::drain_until(
            future::pending::<Result<()>>(),
            &Budget::new(CONNECT_BUDGET),
            &mut input,
        )
        .await
    });
    for _ in 0..CHANNEL_QUEUE {
        input_tx
            .send(budget.hold(Bytes::from(vec![0; READ_BUFFER])).await)
            .await?;
    }
    wait_for(|| budget.available() == CHANNEL_BUFFER as usize)
        .await
        .context("budget not given back")?;
    connecting.abort();
    Ok(())
}

#[tokio::test]
async fn connecting_streams_stop_taking_data_without_connect_budget() -> Result<()> {
    // A port which never answers, whose client sends more than the
    // connect budget holds.
    let budget = Budget::new(CHANNEL_BUFFER);
    let connect_budget = Budget::new(4 * u32::try_from(READ_BUFFER)?);
    let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
    let connecting = tokio::spawn({
        let connect_budget = connect_budget.clone();
        async move {
            PortForward::drain_until(future::pending::<Result<()>>(), &connect_budget, &mut input)
                .await
        }
    });
    for _ in 0..8 {
        input_tx
            .send(budget.hold(Bytes::from(vec![0; READ_BUFFER])).await)
            .await?;
    }

    // The connect budget took what it holds, the rest stays in the byte
    // budget of the client, one message of it taken already.
    wait_for(|| {
        connect_budget.available() == 0
            && budget.available() == CHANNEL_BUFFER as usize - 4 * READ_BUFFER
    })
    .await
    .context("budgets not taken")?;
    time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        budget.available(),
        CHANNEL_BUFFER as usize - 4 * READ_BUFFER
    );

    // A connection which ends gives its share back.
    connecting.abort();
    let _ = connecting.await;
    assert_eq!(connect_budget.available(), 4 * READ_BUFFER);
    Ok(())
}

#[tokio::test]
async fn parallel_connections_all_get_established() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let namespace = Namespace::with_linger(NETNS.into(), Duration::from_secs(60));
    let accept = tokio::spawn(async move {
        let mut connections = vec![];
        for _ in 0..64 {
            connections.push(listener.accept().await?);
        }
        anyhow::Ok(connections)
    });
    let mut connects = JoinSet::new();
    for i in 0..64u8 {
        let namespace = namespace.clone();
        connects.spawn(async move {
            let (input_tx, mut input) = mpsc::channel::<Held>(1);
            input_tx.send(Bytes::from(vec![i]).into()).await?;
            let (_connection, early) = PortForward::establish(&namespace, port, &mut input).await?;
            anyhow::Ok(early.data.len() + early.last.map_or(0, |data| data.len()))
        });
    }
    while let Some(result) = connects.join_next().await {
        result??;
    }
    assert_eq!(
        time::timeout(Duration::from_secs(5), accept).await???.len(),
        64
    );
    assert_eq!(
        namespace.connect_budget.available(),
        CONNECT_BUDGET as usize
    );
    Ok(())
}

#[tokio::test]
async fn too_much_input_while_connecting_fails() -> Result<()> {
    let (input_tx, mut input) = mpsc::channel(4);
    tokio::spawn(async move {
        while input_tx
            .send(Bytes::from(vec![0; READ_BUFFER]).into())
            .await
            .is_ok()
        {}
    });
    let err = PortForward::drain_until(
        future::pending::<Result<()>>(),
        &Budget::new(CONNECT_BUDGET),
        &mut input,
    )
    .await
    .expect_err("overflow");
    assert!(
        err.to_string()
            .contains("before the connection got established"),
        "{err:#}"
    );
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
async fn namespace_threads_serve_close_connections() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let namespace = Namespace::with_linger(NETNS.into(), Duration::from_secs(60));
    for _ in 0..3 {
        let _connection = PortForward::connect(&namespace, port).await?;
        listener.accept().await?;
    }
    assert_eq!(namespace.threads.load(Ordering::Relaxed), 1);

    // The thread ends without connections, the next one starts another.
    let namespace = Namespace::with_linger(NETNS.into(), Duration::from_millis(10));
    let _connection = PortForward::connect(&namespace, port).await?;
    wait_for(|| namespace.ended.load(Ordering::Relaxed) != 0)
        .await
        .context("the thread lingers")?;
    let _connection = PortForward::connect(&namespace, port).await?;
    assert_eq!(namespace.threads.load(Ordering::Relaxed), 2);
    Ok(())
}

#[tokio::test]
async fn namespace_threads_end_with_their_session() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let namespace = Namespace::with_linger(NETNS.into(), Duration::from_secs(60));
    let _connection = PortForward::connect(&namespace, listener.local_addr()?.port()).await?;
    let ended = namespace.ended.clone();
    drop(namespace);
    wait_for(|| ended.load(Ordering::Relaxed) != 0)
        .await
        .context("the thread lingers")?;
    Ok(())
}

#[tokio::test]
async fn every_connection_gets_its_namespace_error() -> Result<()> {
    let namespace = Namespace::new("/nonexistent".into());
    let connects = (0..10).map(|_| PortForward::connect(&namespace, 80));
    for result in futures::future::join_all(connects).await {
        let err = result.expect_err("no namespace");
        assert!(
            err.chain().any(|e| e.downcast_ref::<io::Error>().is_some()),
            "{err:#}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn namespace_failures_are_not_kept() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("netns");
    let namespace = Namespace::new(path.clone());
    let err = PortForward::connect(&namespace, 80)
        .await
        .expect_err("no namespace");
    // The error keeps its chain.
    assert!(
        err.chain().any(|e| e.downcast_ref::<io::Error>().is_some()),
        "{err:#}"
    );
    assert!(
        format!("{err:#}").contains(&format!("open network namespace {}", path.display())),
        "{err:#}"
    );

    // Once the namespace exists, the next connection works.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    std::os::unix::fs::symlink(NETNS, &path)?;
    let _connection = PortForward::connect(&namespace, listener.local_addr()?.port()).await?;
    Ok(())
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
