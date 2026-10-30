use super::*;
use crate::{
    random_id::RandomId,
    streaming_server::{
        ADDR, ATTACH_PATH, AttachSession, EXEC_PATH, PORT_FORWARD_PATH, StreamingServer,
        remotecommand::SPDY_PROTOCOLS,
        spdy::testing::{Client, Frame, Headers},
        testing::*,
    },
};
use anyhow::{Context, Result};
use hyper::{
    HeaderMap, StatusCode,
    header::{self, HeaderValue},
};
use tokio::{io::AsyncWriteExt, net::TcpStream, task::LocalSet};

#[test]
fn upgrade_headers_count_in_all_values() {
    let mut headers = HeaderMap::new();
    headers.append(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.append(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.append(header::UPGRADE, HeaderValue::from_static("spdy/3.1"));
    headers.append(
        PROTOCOL_VERSION_HEADER,
        HeaderValue::from_static("v5.channel.k8s.io"),
    );
    headers.append(
        PROTOCOL_VERSION_HEADER,
        HeaderValue::from_static("v4.channel.k8s.io"),
    );
    assert!(matches!(
        StreamingServer::spdy_handshake(&headers, &[("v4.channel.k8s.io", ())]),
        Ok(((), "v4.channel.k8s.io"))
    ));
    assert!(!StreamingServer::is_websocket_request(&headers));

    headers.append(header::UPGRADE, HeaderValue::from_static("h2c, WebSocket"));
    assert!(StreamingServer::is_websocket_request(&headers));

    // A token which only contains the protocol is none.
    let mut headers = HeaderMap::new();
    headers.append(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.append(header::UPGRADE, HeaderValue::from_static("SPDY/3.10"));
    let Err(response) = StreamingServer::spdy_handshake(&headers, &[("v4.channel.k8s.io", ())])
    else {
        panic!("upgraded to SPDY/3.10");
    };
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Send a port forward request with the extra headers, return the
/// response head and the connection.
async fn port_forward_request(
    server: &StreamingServer,
    url: &str,
    headers: &str,
) -> Result<(String, TcpStream)> {
    let path = url
        .strip_prefix(&format!("http://{ADDR}:{}", server.port()))
        .context("url")?;
    let mut stream = TcpStream::connect((ADDR, server.port())).await?;
    stream
        .write_all(
            format!("POST {path} HTTP/1.1\r\nHost: {ADDR}\r\nContent-Length: 0\r\n{headers}\r\n")
                .as_bytes(),
        )
        .await?;
    Ok((response_head(&mut stream).await?, stream))
}

const SPDY_UPGRADE: &str = "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n";

/// A valid SPDY port forward upgrade.
const SPDY_PORT_FORWARD: &str = "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                                 X-Stream-Protocol-Version: portforward.k8s.io\r\n";

#[tokio::test]
async fn port_forward_upgrades_to_spdy() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let server = StreamingServer::default();
            let url = server
                .port_forward_url("/proc/self/ns/net".into(), None)
                .await?;

            let (response, stream) = port_forward_request(
                &server,
                &url,
                &format!("{SPDY_UPGRADE}X-Stream-Protocol-Version: v4.channel.k8s.io, portforward.k8s.io\r\n"),
            )
            .await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            assert!(response.contains("upgrade: spdy/3.1"), "{response}");
            // Only other responses end the connection.
            assert!(!response.contains("close"), "{response}");
            assert!(
                response.contains("x-stream-protocol-version: portforward.k8s.io"),
                "{response}"
            );

            // The connection speaks SPDY now, a stream without headers
            // gets rejected.
            let mut client = Client::new(stream)?;
            client.open(1, Headers::default()).await?;
            assert!(matches!(
                client.frame().await?,
                Some(Frame::RstStream { stream_id: 1, .. })
            ));

            // The token got used, also a request with bad headers learns
            // that first.
            let (response, _) = port_forward_request(&server, &url, SPDY_UPGRADE).await?;
            assert!(response.starts_with("http/1.1 404"), "{response}");
            Ok(())
        })
        .await
}

#[tokio::test]
async fn port_forward_tokens_survive_rejected_requests() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let server = StreamingServer::default();
            // A SPDY request without the protocol, then a valid one.
            let url = server
                .port_forward_url("/proc/self/ns/net".into(), None)
                .await?;
            let (response, _) = port_forward_request(&server, &url, SPDY_UPGRADE).await?;
            assert!(response.starts_with("http/1.1 400"), "{response}");
            let (response, _) = port_forward_request(&server, &url, SPDY_PORT_FORWARD).await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");

            // A WebSocket request without ports, then a valid one.
            let url = server
                .port_forward_url("/proc/self/ns/net".into(), None)
                .await?;
            let (response, _) = port_forward_websocket_to(&server, &url, "", "").await?;
            assert!(response.starts_with("http/1.1 400"), "{response}");
            let (response, _) = port_forward_websocket_to(&server, &url, "?port=80", "").await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            Ok(())
        })
        .await
}

#[tokio::test]
async fn port_forward_rejects_other_upgrades() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let server = StreamingServer::default();
            for (headers, status) in [
                (
                    "Connection: Upgrade\r\nUpgrade: websocket\r\n\
                     X-Stream-Protocol-Version: portforward.k8s.io\r\n"
                        .to_owned(),
                    "400",
                ),
                (SPDY_UPGRADE.to_owned(), "400"),
                (
                    format!("{SPDY_UPGRADE}X-Stream-Protocol-Version: v4.channel.k8s.io\r\n"),
                    "403",
                ),
            ] {
                let url = server
                    .port_forward_url("/proc/self/ns/net".into(), None)
                    .await?;
                let (response, _) = port_forward_request(&server, &url, &headers).await?;
                assert!(
                    response.starts_with(&format!("http/1.1 {status}")),
                    "{response}"
                );
                if status == "403" {
                    assert!(
                        response
                            .contains("x-accepted-stream-protocol-versions: portforward.k8s.io"),
                        "{response}"
                    );
                }
            }

            // Exec and attach tokens are no port forward tokens.
            let (_, child, _) = attach_child(false)?;
            let url = server
                .attach_url(AttachSession {
                    container_id: "id".into(),
                    child,
                    stdin: false,
                    stdout: true,
                    stderr: false,
                    stop_after_stdin_eof: false,
                    idle_timeout: None,
                })
                .await?
                .replace(ATTACH_PATH, PORT_FORWARD_PATH);
            let (response, _) = port_forward_request(&server, &url, SPDY_PORT_FORWARD).await?;
            assert!(response.starts_with("http/1.1 404"), "{response}");
            Ok(())
        })
        .await
}

#[tokio::test]
async fn sessions_work_on_their_path_only() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let server = StreamingServer::default();
            let (_, child, _token) = attach_child(false)?;
            let url = server
                .attach_url(AttachSession {
                    container_id: "id".into(),
                    child,
                    stdin: false,
                    stdout: true,
                    stderr: false,
                    stop_after_stdin_eof: false,
                    idle_timeout: None,
                })
                .await?;

            // An attach token is no exec token, and it stays for its
            // own path.
            let exec = url.replace(ATTACH_PATH, EXEC_PATH);
            let (response, _) = upgrade(&server, &exec, "POST", SPDY_EXEC_UPGRADE).await?;
            assert!(response.starts_with("http/1.1 404"), "{response}");
            let (response, _) = upgrade(&server, &url, "POST", SPDY_EXEC_UPGRADE).await?;
            assert!(response.starts_with("http/1.1 101"), "{response}");
            Ok(())
        })
        .await
}

#[tokio::test]
async fn spdy_upgrades_get_negotiated() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let server = StreamingServer::default();
            for (headers, status) in [
                ("Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n", "400"),
                (
                    "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                     X-Stream-Protocol-Version: v5.channel.k8s.io\r\n",
                    "403",
                ),
                ("", "400"),
            ] {
                let (_, child, _token) = attach_child(false)?;
                let url = server
                    .attach_url(AttachSession {
                        container_id: "id".into(),
                        child,
                        stdin: false,
                        stdout: true,
                        stderr: false,
                        stop_after_stdin_eof: false,
                        idle_timeout: None,
                    })
                    .await?;
                let (response, _) = upgrade(&server, &url, "POST", headers).await?;
                assert!(
                    response.starts_with(&format!("http/1.1 {status}")),
                    "{response}"
                );
                if status == "403" {
                    for protocol in SPDY_PROTOCOLS.map(|(p, _)| p) {
                        assert!(
                            response.contains(&format!(
                                "x-accepted-stream-protocol-versions: {protocol}"
                            )),
                            "{response}"
                        );
                    }
                }
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn rejected_upgrades_keep_the_token() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let server = StreamingServer::default();
            // An unknown token comes before the protocol.
            let (_, child, _token) = attach_child(false)?;
            let url = server
                .attach_url(AttachSession {
                    container_id: "id".into(),
                    child,
                    stdin: false,
                    stdout: true,
                    stderr: false,
                    stop_after_stdin_eof: false,
                    idle_timeout: None,
                })
                .await?;
            let unknown = url.replace(
                url.rsplit('/').next().context("token")?,
                &RandomId::new().to_string(),
            );
            let (response, _) = upgrade(&server, &unknown, "POST", "").await?;
            assert!(response.starts_with("http/1.1 404"), "{response}");

            for (rejected, status, valid) in [
                (
                    "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                     X-Stream-Protocol-Version: v5.channel.k8s.io\r\n"
                        .to_owned(),
                    "403",
                    SPDY_EXEC_UPGRADE.to_owned(),
                ),
                ("".to_owned(), "400", SPDY_EXEC_UPGRADE.to_owned()),
                (
                    websocket_headers("v3.channel.k8s.io"),
                    "403",
                    websocket_headers(PROTOCOL_V5),
                ),
            ] {
                let (_, child, _token) = attach_child(false)?;
                let url = server
                    .attach_url(AttachSession {
                        container_id: "id".into(),
                        child,
                        stdin: false,
                        stdout: true,
                        stderr: false,
                        stop_after_stdin_eof: false,
                        idle_timeout: None,
                    })
                    .await?;
                let (response, _) = upgrade(&server, &url, "POST", &rejected).await?;
                assert!(
                    response.starts_with(&format!("http/1.1 {status}")),
                    "{response}"
                );

                // The client can retry with a request the server takes,
                // which uses the token.
                let method = if valid.contains("websocket") {
                    "GET"
                } else {
                    "POST"
                };
                let (response, _) = upgrade(&server, &url, method, &valid).await?;
                assert!(response.starts_with("http/1.1 101"), "{response}");
                let (response, _) = upgrade(&server, &url, method, &valid).await?;
                assert!(response.starts_with("http/1.1 404"), "{response}");
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn unsupported_websocket_protocols_get_rejected() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (_, _server, _, _, response) = websocket_attach("v3.channel.k8s.io").await?;
            assert!(response.starts_with("http/1.1 403"), "{response}");
            Ok(())
        })
        .await
}
