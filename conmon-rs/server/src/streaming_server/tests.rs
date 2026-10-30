use crate::{
    random_id::RandomId,
    streaming_server::{
        ADDR, HEADER_READ_TIMEOUT, MAX_CONNECTIONS, MAX_HEADER_BUF_SIZE, SWEEP_INTERVAL,
        StreamingServer, TOO_MANY_REQUESTS, UPGRADE_TIMEOUT,
        bounded_map::DEFAULT_CAPACITY,
        remotecommand::STDIN_BYTE,
        testing::{attach_session, client_frame, response_head},
    },
};
use anyhow::{Context, Result};

use std::{io, path::PathBuf};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    task::LocalSet,
    time::{self, Duration},
};

#[tokio::test]
async fn connections_without_a_request_get_closed() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            let mut stream = TcpStream::connect((ADDR, server.port)).await?;
            let mut rest = vec![];
            time::timeout(4 * HEADER_READ_TIMEOUT, stream.read_to_end(&mut rest))
                .await
                .context("connection still open")??;
            Ok(())
        })
        .await
}

#[tokio::test]
async fn connections_wait_while_too_many_are_open() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            let mut open = vec![];
            for _ in 0..MAX_CONNECTIONS {
                open.push(TcpStream::connect((ADDR, server.port)).await?);
            }

            // A request on another connection waits for one of them.
            let mut stream = TcpStream::connect((ADDR, server.port)).await?;
            stream
                .write_all(format!("GET /x HTTP/1.1\r\nHost: {ADDR}\r\n\r\n").as_bytes())
                .await?;
            let mut byte = [0];
            let early = time::timeout(HEADER_READ_TIMEOUT / 5, stream.read(&mut byte)).await;
            assert!(early.is_err(), "request got served beyond the limit");
            drop(open.pop());
            let response = time::timeout(Duration::from_secs(5), response_head(&mut stream))
                .await
                .context("request not served after a connection ended")??;
            assert!(response.starts_with("http/1.1 404"), "{response}");
            // The others are still open, the end of one freed its permit,
            // not the header timeout of all of them.
            for held in &open {
                let read = held.try_read(&mut byte);
                assert!(
                    matches!(&read, Err(e) if e.kind() == io::ErrorKind::WouldBlock),
                    "{read:?}"
                );
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn upgraded_sessions_outlive_the_upgrade_timeout() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            // The deadline of a connection ends once hyper handed it over
            // for the upgrade, a session takes as long as it takes.
            let (io, _server, mut stream) = attach_session(true, false).await?;
            time::sleep(UPGRADE_TIMEOUT + Duration::from_millis(500)).await;
            stream
                .write_all(&client_frame(0x2, &[STDIN_BYTE, b'x']))
                .await?;
            let attach = io.attach().await;
            let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
            assert_eq!(data, b"x".as_slice());
            Ok(())
        })
        .await
}

#[tokio::test]
async fn clients_which_do_not_read_get_one_response() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            // Clients which send requests without end and never read the
            // responses.
            let request = format!("GET /x HTTP/1.1\r\nHost: {ADDR}\r\n\r\n");
            let mut writers = vec![];
            for _ in 0..MAX_CONNECTIONS {
                let mut stream = TcpStream::connect((ADDR, server.port)).await?;
                let request = request.clone();
                writers.push(tokio::spawn(async move {
                    while stream.write_all(request.as_bytes()).await.is_ok() {}
                }));
            }

            // They get closed after their response, which lets others in
            // before any timeout.
            let mut stream = TcpStream::connect((ADDR, server.port)).await?;
            stream.write_all(request.as_bytes()).await?;
            let response = time::timeout(HEADER_READ_TIMEOUT, response_head(&mut stream))
                .await
                .context("request not served while others did not read")??;
            assert!(response.starts_with("http/1.1 404"), "{response}");
            for writer in writers {
                writer.abort();
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn request_heads_are_bounded() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            let request = |padding: usize| {
                format!(
                    "GET /x HTTP/1.1\r\nHost: {ADDR}\r\nX-Padding: {}\r\n\r\n",
                    "a".repeat(padding)
                )
            };
            let respond = async |request: String| -> Result<String> {
                let mut stream = TcpStream::connect((ADDR, server.port)).await?;
                // A server which rejects the head early may close the
                // connection before all of it went out.
                let _ = stream.write_all(request.as_bytes()).await;
                time::timeout(HEADER_READ_TIMEOUT, response_head(&mut stream))
                    .await
                    .context("no response")?
            };

            // Large heads of real clients still get served.
            let response = respond(request(MAX_HEADER_BUF_SIZE / 2)).await?;
            assert!(response.starts_with("http/1.1 404"), "{response}");

            // Larger ones do not get buffered.
            let response = respond(request(2 * MAX_HEADER_BUF_SIZE)).await?;
            assert!(response.starts_with("http/1.1 431"), "{response}");
            Ok(())
        })
        .await
}

#[tokio::test(start_paused = true)]
async fn sessions_wait_for_their_client_in_bounds() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let mut server = StreamingServer::default();
            server.start_if_required().await?;
            // Nothing wakes up the server while no session waits.
            assert!(!server.state.read().await.sweeping);
            let mut urls = vec![];
            for _ in 0..DEFAULT_CAPACITY {
                urls.push(server.port_forward_url(PathBuf::new(), None).await?);
            }
            // A full map fails new sessions instead of dropping one whose
            // client would come.
            let err = server
                .port_forward_url(PathBuf::new(), None)
                .await
                .expect_err("full");
            // The message alone, which the RPC passes on as it is.
            assert_eq!(format!("{err:#}"), TOO_MANY_REQUESTS);
            assert_eq!(
                server.state.read().await.port_forwards.len(),
                DEFAULT_CAPACITY
            );

            // The sessions whose client did not come go without a request,
            // then the sweeps stop until the next session.
            time::sleep(Duration::from_secs(60) + 2 * SWEEP_INTERVAL).await;
            assert_eq!(server.state.read().await.port_forwards.len(), 0);
            assert!(!server.state.read().await.sweeping);
            server.port_forward_url(PathBuf::new(), None).await?;
            assert!(server.state.read().await.sweeping);
            Ok(())
        })
        .await
}

#[test]
fn logged_paths_hide_the_tokens() {
    let token = RandomId::new().to_string();
    let logged = StreamingServer::logged_path(&format!("/exec/{token}"));
    assert_eq!(logged, format!("/\"exec\"/{:?}...", &token[..8]));
    assert!(!logged.contains(&token[..9]));
    assert_eq!(StreamingServer::logged_path("/\n"), r#""/\n""#);
}
