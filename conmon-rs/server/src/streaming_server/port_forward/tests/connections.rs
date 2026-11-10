//! Connecting to the pod, and the input which arrives meanwhile.

use super::*;
use crate::test_util::pattern;

#[tokio::test]
async fn connections_send_without_delay() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let connection = PortForward::connect(&Namespace::new(NETNS.into()), port).await?;
    assert!(connection.nodelay()?);
    Ok(())
}

#[tokio::test]
async fn connections_fall_back_to_ipv6() -> Result<()> {
    // Only IPv6 listens on the port, the connection to IPv4 gets refused.
    // An IPv4 socket which does not listen holds the port, so that no
    // listener of a test in parallel gets it. Another test may hold it
    // already, the next port of IPv6 likely is free.
    let mut bound = None;
    for _ in 0..10 {
        let Ok(listener) = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await else {
            eprintln!("skipped: no IPv6 loopback");
            return Ok(());
        };
        let port = listener.local_addr()?.port();
        let ipv4 = TcpSocket::new_v4()?;
        if ipv4.bind((Ipv4Addr::LOCALHOST, port).into()).is_ok() {
            bound = Some((listener, port, ipv4));
            break;
        }
    }
    let (listener, port, _ipv4) = bound.context("no port free on IPv4 and IPv6")?;
    let mut client = serve()?;
    request(&mut client, port).await?;

    client
        .send(Frame::Data {
            stream_id: 3,
            fin: true,
            data: Bytes::from_static(b"request"),
        })
        .await?;
    let (mut connection, _) = time::timeout(Duration::from_secs(5), listener.accept()).await??;
    let mut request = vec![];
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
    Ok(())
}

#[tokio::test]
async fn input_gets_taken_while_connecting() -> Result<()> {
    let (input_tx, mut input) = mpsc::channel(1);
    let (connected_tx, connected) = oneshot::channel();

    // More messages than the queue holds arrive before the connection.
    let send = task::spawn(async move {
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
async fn small_input_while_connecting_gets_collected() -> Result<()> {
    let connect_budget = Budget::new(CONNECT_BUDGET);
    let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
    let (connected_tx, connected) = oneshot::channel();
    let send = task::spawn({
        let budget = Budget::new(1000);
        async move {
            for byte in pattern(1000) {
                let data = budget.hold(Bytes::from(vec![byte])).await;
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
    assert_eq!(early.data[..], pattern(1000));
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
        wait_for(|| connect_budget.available() == 10 && client_budget.available() == 10).await
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

#[tokio::test]
async fn input_while_connecting_leaves_the_budget() -> Result<()> {
    // Connections to a port which does not answer take what the client
    // sends, which must not stop the other connections of the client.
    let budget = Budget::new(CHANNEL_BUFFER);
    let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
    let connecting = task::spawn(async move {
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

#[tokio::test(start_paused = true)]
async fn connecting_streams_stop_taking_data_without_connect_budget() -> Result<()> {
    // A port which never answers, whose client sends more than the
    // connect budget holds.
    let budget = Budget::new(CHANNEL_BUFFER);
    let connect_budget = Budget::new(4 * u32::try_from(READ_BUFFER)?);
    let (input_tx, mut input) = mpsc::channel(CHANNEL_QUEUE);
    let connecting = task::spawn({
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
    // With the clock paused, the sleep ends only once the connection waits.
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
    let accept = task::spawn(async move {
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
    task::spawn(async move {
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
