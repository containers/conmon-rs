//! The channel protocol over a WebSocket, and the ports of its query.

use super::*;

#[tokio::test]
async fn failed_channels_let_go_of_their_input() -> Result<()> {
    let (_socket, port) = closed_port()?;
    let budget = Budget::new(CHANNEL_BUFFER);
    let (input_tx, input) = mpsc::channel(CHANNEL_QUEUE);
    // A client which does not read, the error waits.
    let (messages, mut output) = mpsc::channel(1);
    messages.send(Message::Binary(Bytes::new())).await?;
    let forward = task::spawn(PortForward::forward_channel(
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

#[tokio::test]
async fn pods_which_reject_the_input_still_answer() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let budget = Budget::new(CHANNEL_BUFFER);
    let (input_tx, input) = mpsc::channel(CHANNEL_QUEUE);
    // A client which does not read yet, the response waits.
    let (messages, mut output) = mpsc::channel(1);
    messages.send(Message::Binary(Bytes::new())).await?;
    let forward = task::spawn(PortForward::forward_channel(
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

    // Like a server which answers an upload it rejects early, and closes the
    // connection with the rest of it unread, which resets it.
    let send = |data: &'static [u8]| {
        let (input_tx, budget) = (&input_tx, &budget);
        async move {
            let data = budget.hold(Bytes::from_static(data)).await;
            input_tx.send(data).await.context("input closed")
        }
    };
    send(b"head").await?;
    let (mut connection, _) = time::timeout(Duration::from_secs(5), listener.accept()).await??;
    let mut head = [0; 4];
    connection.read_exact(&mut head).await?;
    send(b"rest").await?;
    connection.peek(&mut [0; 1]).await?;
    connection.write_all(b"response").await?;
    drop(connection);
    // Writing to the pod fails, at the latest once the reset came back for
    // the first write. Then the input stays queued, and only then the client
    // reads, within the grace period, the response still waits.
    let deadline = time::Instant::now() + Duration::from_secs(5);
    loop {
        send(b"more").await?;
        time::sleep(Duration::from_millis(20)).await;
        if budget.available() < CHANNEL_BUFFER as usize {
            break;
        }
        assert!(
            time::Instant::now() < deadline,
            "writing to the pod never failed"
        );
    }

    output.recv().await;
    let response = output.recv().await.context("no response")?;
    assert_eq!(
        response,
        Message::Binary(Bytes::from_static(b"\x00response"))
    );
    let error = output.recv().await.context("no error")?;
    assert!(
        matches!(&error, Message::Binary(data) if data.starts_with(b"\x01error forwarding port")),
        "{error:?}"
    );
    forward.await?;
    Ok(())
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

#[test]
fn query_components_get_decoded() {
    for (encoded, decoded) in [
        ("", ""),
        ("port", "port"),
        ("%70ort", "port"),
        ("81%2C82%2c83", "81,82,83"),
        // A plus is a space, an escaped one stays.
        ("a+b", "a b"),
        ("a%2Bb", "a+b"),
        ("a%20b", "a b"),
        // Invalid escapes stay as they are, what follows gets decoded.
        ("%", "%"),
        ("80%", "80%"),
        ("%4", "%4"),
        ("%zz", "%zz"),
        ("%4g", "%4g"),
        ("%g1", "%g1"),
        ("%%41", "%A"),
        ("%4%41", "%4A"),
        ("%+41", "% 41"),
        ("%2+", "%2 "),
        ("%-1", "%-1"),
        ("%25", "%"),
        ("%2541", "%41"),
        // Bytes pass as they are, also the ones of several escapes.
        ("%00", "\0"),
        ("é", "é"),
        ("%C3%A9", "é"),
        ("%c3%a9", "é"),
        ("%E2%82%AC", "€"),
        // No UTF-8.
        ("%ff", "\u{fffd}"),
        ("a%C3", "a\u{fffd}"),
        ("%C3+", "\u{fffd} "),
        ("%E2%82", "\u{fffd}"),
        ("%C3%28", "\u{fffd}("),
    ] {
        assert_eq!(PortForward::percent_decode(encoded), decoded, "{encoded:?}");
    }
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
