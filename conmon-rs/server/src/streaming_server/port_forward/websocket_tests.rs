//! Port forwarding over a WebSocket, through the streaming server.

use crate::{
    streaming_server::{
        ADDR, StreamingServer,
        spdy::testing::{Client, Headers},
        testing::*,
    },
    test_util::{closed_port, wait_for},
};
use anyhow::{Context, Result};
use data_encoding::BASE64;
use nix::sys::socket::{setsockopt, sockopt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::LocalSet,
    time::{self, Duration},
};

#[tokio::test]
async fn port_forward_over_websocket_channels() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;

            for base64 in [false, true] {
                // Two ports which listen and one between them which
                // does not.
                let first = tokio::net::TcpListener::bind((ADDR, 0)).await?;
                let (_closed, closed) = closed_port()?;
                let last = tokio::net::TcpListener::bind((ADDR, 0)).await?;
                let ports = [
                    first.local_addr()?.port(),
                    closed,
                    last.local_addr()?.port(),
                ];
                let protocol = if base64 {
                    "v4.base64.channel.k8s.io"
                } else {
                    "v4.channel.k8s.io"
                };
                let (response, mut stream) = port_forward_websocket(
                    &server,
                    &format!("?port={},{}&port={}", ports[0], ports[1], ports[2]),
                    protocol,
                )
                .await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                assert!(
                    response.contains(&format!("sec-websocket-protocol: {protocol}")),
                    "{response}"
                );
                assert!(!response.contains("close"), "{response}");

                // A message of the channel protocol, text for base64.
                let message = |channel: u8, data: &[u8]| {
                    if base64 {
                        let mut text = vec![b'0' + channel];
                        text.extend(BASE64.encode(data).into_bytes());
                        (1, text)
                    } else {
                        (2, [&[channel], data].concat())
                    }
                };
                // The channel of a message and its data.
                let decode = |(opcode, payload): (u8, Vec<u8>)| -> Result<(u8, Vec<u8>)> {
                    let (channel, data) = payload.split_first().context("empty message")?;
                    Ok(if base64 {
                        assert_eq!(opcode, 1);
                        (channel - b'0', BASE64.decode(data)?)
                    } else {
                        assert_eq!(opcode, 2);
                        (*channel, data.to_vec())
                    })
                };

                // Port i gets the data channel 2i and the error channel
                // 2i+1, each starts with the port. The port which does
                // not listen reports its failure on its error channel,
                // which may come before the ports after it.
                let mut messages = vec![];
                for _ in 0..7 {
                    messages.push(decode(server_frame(&mut stream).await?)?);
                }
                let error = messages
                    .iter()
                    .position(|(channel, data)| *channel == 3 && data.len() > 2)
                    .context("no error of the closed port")?;
                let (_, error) = messages.remove(error);
                let error = String::from_utf8(error)?;
                assert!(
                    error.starts_with(&format!("error forwarding port {closed}:")),
                    "{error}"
                );
                let expected: Vec<_> = (0..6)
                    .map(|channel| {
                        (
                            channel,
                            ports[usize::from(channel / 2)].to_le_bytes().to_vec(),
                        )
                    })
                    .collect();
                assert_eq!(messages, expected);

                // Each data channel goes to its own port.
                for (channel, listener) in [(0, &first), (4, &last)] {
                    let (opcode, payload) = message(channel, &[b'0' + channel]);
                    stream.write_all(&client_frame(opcode, &payload)).await?;
                    let (mut connection, _) = listener.accept().await?;
                    let mut request = [0];
                    connection.read_exact(&mut request).await?;
                    assert_eq!(request, [b'0' + channel]);
                    connection.write_all(b"response").await?;
                    drop(connection);
                    assert_eq!(
                        server_frame(&mut stream).await?,
                        message(channel, b"response")
                    );
                }

                // The last port is done, which ends the session.
                assert_eq!(server_frame(&mut stream).await?.0, 8);
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn port_forward_over_websocket_ends_idle_sessions_with_stuck_ports() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            // A pod which accepts, but never reads.
            let listener = tokio::net::TcpListener::bind((ADDR, 0)).await?;
            let port = listener.local_addr()?.port();
            let accepted = tokio::spawn(async move { listener.accept().await });

            let (response, stream) = port_forward_websocket_with(
                &server,
                &format!("?port={port}"),
                "",
                Some(Duration::from_millis(500)),
            )
            .await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            let (mut reader, mut writer) = stream.into_split();
            server_frame(&mut reader).await?;
            server_frame(&mut reader).await?;
            let _connection = accepted.await??;

            // More than the socket buffers and the port queue hold.
            let flood = tokio::spawn(async move {
                let frame = client_frame(2, &[&[0], &[0; 60_000][..]].concat());
                while writer.write_all(&frame).await.is_ok() {}
            });
            let (opcode, _) = time::timeout(Duration::from_secs(10), server_frame(&mut reader))
                .await
                .context("no close of the idle session")??;
            assert_eq!(opcode, 8);
            flood.abort();
            Ok(())
        })
        .await
}

#[tokio::test]
async fn port_forward_over_websocket_ends_with_its_forwards() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            // A pod which closes the connection right away.
            let listener = tokio::net::TcpListener::bind((ADDR, 0)).await?;
            let port = listener.local_addr()?.port();
            let accepted = tokio::spawn(async move {
                let (connection, _) = listener.accept().await?;
                drop(connection);
                anyhow::Ok(())
            });

            let (response, stream) =
                port_forward_websocket_with(&server, &format!("?port={port}"), "", None).await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            let (mut reader, mut writer) = stream.into_split();
            server_frame(&mut reader).await?;
            server_frame(&mut reader).await?;
            accepted.await??;

            // The client keeps sending, the session still ends with the
            // forward of its only port.
            let flood = tokio::spawn(async move {
                let frame = client_frame(2, &[0, b'x']);
                while writer.write_all(&frame).await.is_ok() {}
            });
            time::timeout(Duration::from_secs(10), async {
                loop {
                    let (opcode, _) = server_frame(&mut reader).await?;
                    if opcode == 8 {
                        return anyhow::Ok(());
                    }
                }
            })
            .await
            .context("no close after the forward ended")??;
            flood.abort();
            Ok(())
        })
        .await
}

#[tokio::test]
async fn port_forward_over_websocket_waits_for_the_close_answer() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            let (_socket, port) = closed_port()?;
            let (response, mut stream) =
                port_forward_websocket(&server, &format!("?port={port}"), "").await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            server_frame(&mut stream).await?;
            server_frame(&mut stream).await?;

            // The port fails before the client sent anything, the error
            // still reaches it.
            let (opcode, error) = server_frame(&mut stream).await?;
            assert_eq!((opcode, error[0]), (2, 1));
            let error = String::from_utf8_lossy(&error[1..]);
            assert!(error.contains("error forwarding port"), "{error}");
            assert_eq!(server_frame(&mut stream).await?.0, 8);
            answer_close(&mut stream).await
        })
        .await
}

#[tokio::test]
async fn port_forward_over_websocket_pings_do_not_keep_sessions_active() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            let listener = tokio::net::TcpListener::bind((ADDR, 0)).await?;
            let port = listener.local_addr()?.port();
            let accepted = tokio::spawn(async move { listener.accept().await });

            let (response, stream) = port_forward_websocket_with(
                &server,
                &format!("?port={port}"),
                "",
                Some(Duration::from_millis(500)),
            )
            .await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            let (mut reader, mut writer) = stream.into_split();
            server_frame(&mut reader).await?;
            server_frame(&mut reader).await?;
            let _connection = accepted.await??;

            // A heartbeat like the one of client-go.
            let heartbeat = tokio::spawn(async move {
                for opcode in [0x9, 0xa].into_iter().cycle() {
                    time::sleep(Duration::from_millis(100)).await;
                    if writer.write_all(&client_frame(opcode, &[])).await.is_err() {
                        return;
                    }
                }
            });
            time::timeout(Duration::from_secs(10), async {
                loop {
                    // The pongs which answer the pings come first.
                    let (opcode, _) = server_frame(&mut reader).await?;
                    if opcode == 8 {
                        return anyhow::Ok(());
                    }
                    assert_eq!(opcode, 0xa);
                }
            })
            .await
            .context("no close of the idle session")??;
            heartbeat.abort();
            Ok(())
        })
        .await
}

#[tokio::test]
async fn port_forward_over_websocket_reports_errors() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            let (_socket, port) = closed_port()?;

            let (response, mut stream) =
                port_forward_websocket(&server, &format!("?port={port}"), "").await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            server_frame(&mut stream).await?;
            server_frame(&mut stream).await?;

            let (opcode, payload) = server_frame(&mut stream).await?;
            assert_eq!((opcode, payload[0]), (2, 1));
            let error = String::from_utf8_lossy(&payload[1..]).into_owned();
            assert!(
                error.starts_with(&format!(
                    "error forwarding port {port}: failed to connect to localhost:{port}"
                )),
                "{error}"
            );
            assert_eq!(server_frame(&mut stream).await?.0, 8);
            Ok(())
        })
        .await
}

#[tokio::test]
async fn port_forward_over_websocket_closes_on_too_large_messages() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            // A pod which accepts, but never sends.
            let listener = tokio::net::TcpListener::bind((ADDR, 0)).await?;
            let port = listener.local_addr()?.port();
            let accepted = tokio::spawn(async move { listener.accept().await });

            // The channel protocol, after the ports of the channels.
            let (response, mut stream) =
                port_forward_websocket(&server, &format!("?port={port}"), "").await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            server_frame(&mut stream).await?;
            server_frame(&mut stream).await?;
            let _connection = accepted.await??;
            let (reader, writer) = send_too_large(stream);
            too_large_close(reader, writer).await?;

            // The tunnel, which carries SPDY.
            let (response, stream) =
                port_forward_websocket(&server, "", "SPDY/3.1+portforward.k8s.io").await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            let (reader, writer) = send_too_large(stream);
            too_large_close(reader, writer).await
        })
        .await
}

#[tokio::test]
async fn port_forward_closes_on_too_large_messages_of_clients_which_do_not_read() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            // A pod which keeps sending.
            let listener = tokio::net::TcpListener::bind((ADDR, 0)).await?;
            let port = listener.local_addr()?.port();
            let pod = tokio::spawn(async move {
                let (mut connection, _) = listener.accept().await?;
                let data = vec![b'o'; 64 * 1024];
                loop {
                    connection.write_all(&data).await?;
                }
                #[expect(unreachable_code, reason = "the type of the task")]
                anyhow::Ok(())
            });

            let (response, mut stream) =
                port_forward_websocket(&server, &format!("?port={port}"), "").await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            // A small buffer, so that the message does not fit into it.
            setsockopt(&stream, sockopt::SndBuf, &4096)?;
            server_frame(&mut stream).await?;
            server_frame(&mut stream).await?;

            // The client does not read while the pod sends, until the
            // server waits for it, then writes all of its message
            // before it reads.
            time::sleep(Duration::from_secs(1)).await;
            let (reader, writer) = send_too_large(stream);
            let written = wait_for(|| writer.is_finished()).await;
            pod.abort();
            written.context("the server does not take the rest of the message")?;
            too_large_close(reader, writer).await
        })
        .await
}

#[tokio::test]
async fn port_forward_over_websocket_rejects_invalid_requests() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            for (query, protocols, status) in [
                ("", "v4.channel.k8s.io", "400"),
                ("?port=0", "", "400"),
                ("?port=80", "v5.channel.k8s.io", "403"),
            ] {
                let (response, _) = port_forward_websocket(&server, query, protocols).await?;
                assert!(
                    response.starts_with(&format!("http/1.1 {status}")),
                    "{response}"
                );
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn port_forward_over_websocket_tunnels_spdy() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            // The tunnel needs no ports, they are in the SPDY streams.
            let (response, mut stream) = port_forward_websocket(
                &server,
                "",
                "v4.channel.k8s.io, SPDY/3.1+portforward.k8s.io",
            )
            .await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            assert!(
                response.contains("sec-websocket-protocol: spdy/3.1+portforward.k8s.io"),
                "{response}"
            );

            // A stream without headers, which gets rejected.
            let (mut client, mut encoded) = Client::pair()?;
            client.open(1, Headers::default()).await?;
            let mut syn_stream = vec![0; 1024];
            let len = encoded.read(&mut syn_stream).await?;
            stream
                .write_all(&client_frame(2, &syn_stream[..len]))
                .await?;

            let (opcode, payload) = server_frame(&mut stream).await?;
            assert_eq!(opcode, 2);
            assert_eq!(
                payload,
                [
                    0x80, 0x03, 0, 0x03, 0, 0, 0, 0x08, 0, 0, 0, 0x01, 0, 0, 0, 0x05
                ]
            );
            Ok(())
        })
        .await
}
