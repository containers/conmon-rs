use crate::{
    mutex::lock,
    random_id::RandomId,
    streaming_server::{
        ADDR, HEADER_READ_TIMEOUT, MAX_CONNECTIONS, MAX_HEADER_BUF_SIZE, SWEEP_INTERVAL,
        StreamingServer, TOO_MANY_REQUESTS, UPGRADE_TIMEOUT,
        bounded_map::DEFAULT_CAPACITY,
        remotecommand::STDIN_BYTE,
        testing::{
            SPDY_EXEC_UPGRADE, attach_child, attach_config, attach_session, client_frame,
            response_head, upgrade,
        },
    },
};
use anyhow::{Context, Result};

use std::{io, path::PathBuf};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{self, LocalSet},
    time::{self, Duration},
};

#[tokio::test]
async fn active_sessions_share_a_budget_and_release_it_on_disconnect() -> Result<()> {
    use super::{
        ExecSession, SessionCommon,
        testing::{PROTOCOL_V5, websocket_headers},
    };
    use crate::{config::Config, conmon_capnp::conmon::CgroupManager, test_util::wait_for};
    use std::sync::Arc;

    LocalSet::new()
        .run_until(async {
            for headers in [websocket_headers(PROTOCOL_V5), SPDY_EXEC_UPGRADE.to_owned()] {
                let server = StreamingServer::new(1);
                let (_io, child, _stop) = attach_child(false)?;
                let url = server
                    .attach_url(attach_config(child, true, false, None))
                    .await?;
                let (response, held) = upgrade(&server, &url, "GET", &headers).await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                assert_eq!(lock(&server.state).active.available_permits(), 0);

                // Taking a token frees a pending slot, but must not free the
                // active budget. All kinds of session and both transports
                // reject new upgrades without consuming their tokens.
                let (_other_io, child, _other_stop) = attach_child(false)?;
                let attach_url = server
                    .attach_url(attach_config(child, true, false, None))
                    .await?;
                let port_url = server
                    .port_forward_url("/proc/self/ns/net".into(), None)
                    .await?;
                let exec_url = server
                    .exec_url(ExecSession {
                        child_reaper: Arc::default(),
                        tty: false,
                        server_config: Arc::new(Config::parse_for_test(["--runtime", "/bin/true"])),
                        cgroup_manager: CgroupManager::Cgroupfs,
                        command: vec!["true".into()],
                        common: SessionCommon {
                            container_id: "id".into(),
                            stdin: true,
                            stdout: true,
                            stderr: false,
                            idle_timeout: None,
                        },
                    })
                    .await?;
                let port_headers = [
                    websocket_headers("SPDY/3.1+portforward.k8s.io"),
                    concat!(
                        "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n",
                        "X-Stream-Protocol-Version: portforward.k8s.io\r\n",
                    )
                    .to_owned(),
                ];
                let session_headers =
                    [websocket_headers(PROTOCOL_V5), SPDY_EXEC_UPGRADE.to_owned()];
                for (url, attempts) in [
                    (&attach_url, session_headers.to_vec()),
                    (&exec_url, session_headers.to_vec()),
                    (&port_url, port_headers.to_vec()),
                ] {
                    for attempt in attempts {
                        let (response, _) = upgrade(&server, url, "GET", &attempt).await?;
                        assert!(response.starts_with("http/1.1 429"), "{response}");
                    }
                }
                assert_eq!(lock(&server.state).sessions.len(), 2);
                assert_eq!(lock(&server.state).port_forwards.len(), 1);

                drop(held);
                wait_for(|| lock(&server.state).active.available_permits() == 1).await?;
                let (response, _retried) = upgrade(&server, &attach_url, "GET", &headers).await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn invalid_upgrades_do_not_consume_active_slots() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let server = StreamingServer::new(1);
            let (_io, child, _stop) = attach_child(false)?;
            let url = server
                .attach_url(attach_config(child, true, false, None))
                .await?;
            let (response, _) = upgrade(&server, &url, "GET", "").await?;
            assert!(response.starts_with("http/1.1 400"), "{response}");
            assert_eq!(lock(&server.state).active.available_permits(), 1);
            let (response, _client) = upgrade(&server, &url, "GET", SPDY_EXEC_UPGRADE).await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            Ok(())
        })
        .await
}

#[tokio::test]
async fn accepted_connections_have_no_delay() -> Result<()> {
    let listener = TcpListener::bind((ADDR, 0)).await?;
    let client = TcpStream::connect(listener.local_addr()?);
    let (accepted, _client) = tokio::try_join!(StreamingServer::accept(&listener), client)?;
    assert!(accepted.nodelay()?);
    Ok(())
}

#[tokio::test]
async fn connections_without_a_request_get_closed() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let server = StreamingServer::default();
            server.start_if_required().await?;
            let mut stream = TcpStream::connect((ADDR, server.port())).await?;
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
            let server = StreamingServer::default();
            server.start_if_required().await?;
            let mut open = vec![];
            for _ in 0..MAX_CONNECTIONS {
                open.push(TcpStream::connect((ADDR, server.port())).await?);
            }

            // A request on another connection waits for one of them.
            let mut stream = TcpStream::connect((ADDR, server.port())).await?;
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
            let attach = io.attach().clone();
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
            let server = StreamingServer::default();
            server.start_if_required().await?;
            // Clients which send requests without end and never read the
            // responses.
            let request = format!("GET /x HTTP/1.1\r\nHost: {ADDR}\r\n\r\n");
            let mut writers = vec![];
            for _ in 0..MAX_CONNECTIONS {
                let mut stream = TcpStream::connect((ADDR, server.port())).await?;
                let request = request.clone();
                writers.push(task::spawn(async move {
                    while stream.write_all(request.as_bytes()).await.is_ok() {}
                }));
            }

            // They get closed after their response, which lets others in
            // before any timeout.
            let mut stream = TcpStream::connect((ADDR, server.port())).await?;
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
            let server = StreamingServer::default();
            server.start_if_required().await?;
            let request = |padding: usize| {
                format!(
                    "GET /x HTTP/1.1\r\nHost: {ADDR}\r\nX-Padding: {}\r\n\r\n",
                    "a".repeat(padding)
                )
            };
            let respond = async |request: String| -> Result<String> {
                let mut stream = TcpStream::connect((ADDR, server.port())).await?;
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

#[tokio::test]
async fn requests_get_routed_by_path_and_method() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let server = StreamingServer::default();
            server.start_if_required().await?;
            let respond = async |method: &str, path: &str| -> Result<String> {
                let mut stream = TcpStream::connect((ADDR, server.port())).await?;
                let request = format!("{method} {path} HTTP/1.1\r\nHost: {ADDR}\r\n\r\n");
                stream.write_all(request.as_bytes()).await?;
                time::timeout(HEADER_READ_TIMEOUT, response_head(&mut stream))
                    .await
                    .context("no response")?
            };
            let token = RandomId::new().to_string();

            // Paths without a kind and a token of one, also for methods
            // which no path takes.
            for path in [
                "/".to_owned(),
                "/exec".to_owned(),
                "/exec/".to_owned(),
                format!("/other/{token}"),
                format!("/attach/{token}/x"),
            ] {
                for method in ["GET", "PUT"] {
                    let response = respond(method, &path).await?;
                    assert!(response.starts_with("http/1.1 404"), "{path}: {response}");
                }
            }

            // Other methods on the path of a session.
            let response = respond("PUT", &format!("/exec/{token}")).await?;
            assert!(response.starts_with("http/1.1 405"), "{response}");
            assert!(
                response.contains("\r\nallow: get,head,post\r\n"),
                "{response}"
            );

            // A token which is none.
            let response = respond("GET", "/portforward/x").await?;
            assert!(response.starts_with("http/1.1 400"), "{response}");

            // A token of no session.
            for method in ["HEAD", "POST"] {
                let response = respond(method, &format!("/exec/{token}")).await?;
                assert!(response.starts_with("http/1.1 404"), "{response}");
            }

            // A HEAD gets the answer of a GET, but no upgrade, which would
            // take the session.
            let (_, child, _token) = attach_child(false)?;
            let url = server
                .attach_url(attach_config(child, true, false, None))
                .await?;
            let (response, _) = upgrade(&server, &url, "HEAD", "").await?;
            assert!(response.starts_with("http/1.1 400"), "{response}");
            let (response, _) = upgrade(&server, &url, "HEAD", SPDY_EXEC_UPGRADE).await?;
            assert!(response.starts_with("http/1.1 405"), "{response}");
            assert!(response.contains("\r\nallow: get,post\r\n"), "{response}");
            let (response, _) = upgrade(&server, &url, "POST", SPDY_EXEC_UPGRADE).await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            Ok(())
        })
        .await
}

#[tokio::test(start_paused = true)]
async fn sessions_wait_for_their_client_in_bounds() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let server = StreamingServer::default();
            // Nothing wakes up the server while no session waits.
            assert!(!lock(&server.state).sweeping);
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
            assert_eq!(lock(&server.state).port_forwards.len(), DEFAULT_CAPACITY);

            // The sessions whose client did not come go without a request,
            // then the sweeps stop until the next session.
            time::sleep(Duration::from_secs(60) + 2 * SWEEP_INTERVAL).await;
            assert_eq!(lock(&server.state).port_forwards.len(), 0);
            assert!(!lock(&server.state).sweeping);
            server.port_forward_url(PathBuf::new(), None).await?;
            assert!(lock(&server.state).sweeping);
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
