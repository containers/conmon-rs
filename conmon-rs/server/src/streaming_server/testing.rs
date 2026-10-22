//! What the tests of the streaming server share.

use crate::{
    child::{Child, ChildKind},
    child_reaper::ReapableChild,
    container_io::{ContainerIO, SharedContainerIO},
    container_log::ContainerLog,
    spdy::testing::Client,
    streaming_server::{ADDR, AttachSession, StreamingServer},
};
use anyhow::{Context, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{self, Duration},
};
use tokio_util::sync::CancellationToken;

pub(super) const PROTOCOL_V5: &str = "v5.channel.k8s.io";

/// A masked client frame, as a WebSocket client sends it.
pub(super) fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [1, 2, 3, 4];
    let mut frame = vec![0x80 | opcode];
    match u16::try_from(payload.len()) {
        Ok(len) if len < 126 => frame.push(0x80 | len as u8),
        Ok(len) => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&len.to_be_bytes());
        }
        Err(_) => panic!("payload too large"),
    }
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    frame
}

/// A container without a terminal and an attach session for it.
pub(super) async fn attach_session(
    stdin: bool,
    stop_after_stdin_eof: bool,
) -> Result<(SharedContainerIO, StreamingServer, TcpStream)> {
    attach_session_with(false, stdin, stop_after_stdin_eof, None).await
}

/// A container and an attach session for it.
pub(super) async fn attach_session_with(
    terminal: bool,
    stdin: bool,
    stop_after_stdin_eof: bool,
    idle_timeout: Option<Duration>,
) -> Result<(SharedContainerIO, StreamingServer, TcpStream)> {
    let io = SharedContainerIO::new(ContainerIO::new(terminal, ContainerLog::new())?);
    let (_, child) = ReapableChild::from_child(Child {
        id: "id".into(),
        kind: ChildKind::Container,
        pid: 0,
        exit_paths: vec![],
        oom_exit_paths: vec![],
        timeout: None,
        io: io.clone(),
        cleanup_cmd: vec![],
        token: CancellationToken::new(),
    });
    let mut server = StreamingServer::default();
    server.start_if_required().await?;
    let url = server
        .attach_url(AttachSession {
            container_id: "id".into(),
            child,
            stdin,
            stdout: true,
            stderr: false,
            stop_after_stdin_eof,
            idle_timeout,
        })
        .await?;
    let path = url
        .strip_prefix(&format!("http://{ADDR}:{}", server.port))
        .context("url")?;

    let mut stream = TcpStream::connect((ADDR, server.port)).await?;
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {ADDR}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Protocol: {PROTOCOL_V5}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    let response = response_head(&mut stream).await?;
    assert!(response.starts_with("http/1.1 101"), "{response}");
    Ok((io, server, stream))
}

/// Read the head of an HTTP response, in lowercase.
pub(super) async fn response_head(stream: &mut TcpStream) -> Result<String> {
    let mut response = vec![];
    while !response.ends_with(b"\r\n\r\n") {
        response.push(stream.read_u8().await?);
    }
    Ok(String::from_utf8_lossy(&response).to_lowercase())
}

/// Read a frame the server sent, its opcode and payload.
pub(super) async fn server_frame<R>(stream: &mut R) -> Result<(u8, Vec<u8>)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0; 2];
    time::timeout(Duration::from_secs(5), stream.read_exact(&mut header)).await??;
    let len = match header[1] & 0x7f {
        126 => usize::from(stream.read_u16().await?),
        127 => usize::try_from(stream.read_u64().await?)?,
        len => usize::from(len),
    };
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).await?;
    Ok((header[0] & 0x0f, payload))
}

/// Answer the close of the server, which waits for it while the client
/// still sends, then ends the connection without a reset.
pub(super) async fn answer_close(stream: &mut TcpStream) -> Result<()> {
    // Input the server does not read would reset the connection.
    stream.write_all(&client_frame(0x2, &[0; 100])).await?;
    let mut byte = [0];
    let early = time::timeout(Duration::from_millis(200), stream.read(&mut byte)).await;
    assert!(early.is_err(), "connection ended before the close answer");
    stream.write_all(&client_frame(0x8, &[])).await?;
    let end = time::timeout(Duration::from_secs(5), stream.read(&mut byte)).await??;
    assert_eq!(end, 0);
    Ok(())
}

/// A container and its child, the token ends the container.
pub(super) fn attach_child(
    terminal: bool,
) -> Result<(SharedContainerIO, ReapableChild, CancellationToken)> {
    let io = SharedContainerIO::new(ContainerIO::new(terminal, ContainerLog::new())?);
    let token = CancellationToken::new();
    let (_, child) = ReapableChild::from_child(Child {
        id: "id".into(),
        kind: ChildKind::Container,
        pid: 0,
        exit_paths: vec![],
        oom_exit_paths: vec![],
        timeout: None,
        io: io.clone(),
        cleanup_cmd: vec![],
        token: token.clone(),
    });
    Ok((io, child, token))
}

/// Send an upgrade request for the URL, return the response head and the
/// connection.
pub(super) async fn upgrade(
    server: &StreamingServer,
    url: &str,
    method: &str,
    headers: &str,
) -> Result<(String, TcpStream)> {
    let path = url
        .strip_prefix(&format!("http://{ADDR}:{}", server.port))
        .context("url")?;
    let mut stream = TcpStream::connect((ADDR, server.port)).await?;
    stream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: {ADDR}\r\nContent-Length: 0\r\n{headers}\r\n"
            )
            .as_bytes(),
        )
        .await?;
    Ok((response_head(&mut stream).await?, stream))
}

pub(super) const SPDY_EXEC_UPGRADE: &str = "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
    X-Stream-Protocol-Version: v5.channel.k8s.io,v4.channel.k8s.io,v3.channel.k8s.io\r\n";

/// An attach session over SPDY with the streams of a client-go v4 client:
/// error 1, stdin 3 and stdout 5.
pub(super) async fn spdy_attach(
    stop_after_stdin_eof: bool,
) -> Result<(SharedContainerIO, StreamingServer, Client<TcpStream>)> {
    spdy_attach_with(false, stop_after_stdin_eof, None).await
}

/// Like [`spdy_attach`], the container may have a terminal, which the
/// client does not ask for.
pub(super) async fn spdy_attach_with(
    terminal: bool,
    stop_after_stdin_eof: bool,
    idle_timeout: Option<Duration>,
) -> Result<(SharedContainerIO, StreamingServer, Client<TcpStream>)> {
    let (io, child, _token) = attach_child(terminal)?;
    let mut server = StreamingServer::default();
    server.start_if_required().await?;
    let url = server
        .attach_url(AttachSession {
            container_id: "id".into(),
            child,
            stdin: true,
            stdout: true,
            stderr: false,
            stop_after_stdin_eof,
            idle_timeout,
        })
        .await?;
    let (response, stream) = upgrade(&server, &url, "POST", SPDY_EXEC_UPGRADE).await?;
    assert!(response.starts_with("http/1.1 101"), "{response}");
    // The server does not speak v5 over SPDY.
    assert!(
        response.contains("x-stream-protocol-version: v4.channel.k8s.io"),
        "{response}"
    );

    let mut client = Client::new(stream)?;
    for (stream_id, typ) in [(1, "error"), (3, "stdin"), (5, "stdout")] {
        client.open(stream_id, [("streamType", typ)].into()).await?;
    }
    Ok((io, server, client))
}

/// An attach session over a WebSocket with the subprotocol, the token
/// ends the container.
pub(super) async fn websocket_attach(
    protocol: &str,
) -> Result<(
    SharedContainerIO,
    StreamingServer,
    TcpStream,
    CancellationToken,
    String,
)> {
    websocket_attach_with(protocol, false).await
}

/// Like [`websocket_attach`], with stdin once.
pub(super) async fn websocket_attach_with(
    protocol: &str,
    stop_after_stdin_eof: bool,
) -> Result<(
    SharedContainerIO,
    StreamingServer,
    TcpStream,
    CancellationToken,
    String,
)> {
    let (io, child, token) = attach_child(false)?;
    let mut server = StreamingServer::default();
    server.start_if_required().await?;
    let url = server
        .attach_url(AttachSession {
            container_id: "id".into(),
            child,
            stdin: true,
            stdout: true,
            stderr: false,
            stop_after_stdin_eof,
            idle_timeout: None,
        })
        .await?;
    let (response, stream) = upgrade(
        &server,
        &url,
        "GET",
        &format!(
            "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: {protocol}\r\n"
        ),
    )
    .await?;
    Ok((io, server, stream, token, response))
}
