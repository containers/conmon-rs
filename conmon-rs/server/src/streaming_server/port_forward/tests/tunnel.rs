//! SPDY tunneled through a WebSocket, and the negotiation of the protocol.

use super::*;

#[tokio::test(start_paused = true)]
async fn tunnels_end_with_clients_which_do_not_read() -> Result<()> {
    let (server, client) = tokio::io::duplex(64 * 1024);
    let (socket, mut client) = tokio::join!(
        WebSocketStream::from_raw_socket(web::ClientIo::new(server), Role::Server, None),
        WebSocketStream::from_raw_socket(client, Role::Client, None),
    );
    let (mut spdy, connection) = tokio::io::duplex(TUNNEL_BUFFER);
    let tunnel = task::spawn(PortForward::tunnel(socket, connection));

    // The SPDY connection sends more than the client reads, then ends.
    let output = vec![0; 8 * TUNNEL_BUFFER];
    assert!(
        time::timeout(Duration::from_secs(1), spdy.write_all(&output))
            .await
            .is_err()
    );
    drop(spdy);

    // The client keeps sending, which the tunnel cannot write anymore.
    let send = task::spawn(async move {
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

/// The close of the client gets its answer right away, also while the SPDY
/// connection does not end, which gets the tunnel stopped once nothing went
/// out for the [`STALL_TIMEOUT`](crate::streaming_server::idle::STALL_TIMEOUT).
#[tokio::test(start_paused = true)]
async fn tunnels_answer_the_close_of_the_client_while_spdy_hangs() -> Result<()> {
    let (server, client) = tokio::io::duplex(64 * 1024);
    let (socket, mut client) = tokio::join!(
        WebSocketStream::from_raw_socket(web::ClientIo::new(server), Role::Server, None),
        WebSocketStream::from_raw_socket(client, Role::Client, None),
    );
    let (_spdy, connection) = tokio::io::duplex(TUNNEL_BUFFER);
    let tunnel = task::spawn(PortForward::tunnel(socket, connection));

    client.close(None).await?;
    let answer = time::timeout(Duration::from_secs(1), client.next())
        .await
        .context("no answer")?;
    assert!(
        matches!(answer, Some(Ok(Message::Close(None)))),
        "{answer:?}"
    );
    time::timeout(Duration::from_secs(120), tunnel)
        .await
        .context("the tunnel hangs")??;
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
