use super::*;
use std::future::poll_fn;

/// The data of a body.
async fn collect(mut body: Body) -> Vec<u8> {
    let mut data = vec![];
    while let Some(Ok(frame)) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        if let Ok(chunk) = frame.into_data() {
            data.extend_from_slice(&chunk);
        }
    }
    data
}

/// A request with the headers, which hyper could upgrade.
fn request(headers: &[(&'static str, &'static str)]) -> Request<()> {
    let mut request = Request::new(());
    for (name, value) in headers {
        request.headers_mut().insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    let on_upgrade = hyper::upgrade::on(Request::new(()));
    request.extensions_mut().insert(on_upgrade);
    request
}

const VALID: [(&str, &str); 4] = [
    ("connection", "keep-alive, Upgrade"),
    ("upgrade", "WebSocket"),
    ("sec-websocket-version", "13"),
    ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
];

#[tokio::test]
async fn upgrades_answer_the_key() {
    let mut request = request(&VALID);
    let Ok(upgrade) = WebSocketUpgrade::from_request(&mut request) else {
        panic!("valid request rejected");
    };
    let response = upgrade
        .protocol(Some("v5.channel.k8s.io"))
        .on_upgrade(|_| async {});
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    let headers = response.headers();
    // The example of RFC 6455.
    assert_eq!(
        headers[header::SEC_WEBSOCKET_ACCEPT],
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
    assert_eq!(headers[header::SEC_WEBSOCKET_PROTOCOL], "v5.channel.k8s.io");
    assert_eq!(headers[header::UPGRADE], "websocket");
    assert_eq!(headers[header::CONNECTION], "upgrade");
}

#[tokio::test]
async fn invalid_upgrades_get_rejected() {
    for (missing, status, message) in [
        (
            "connection",
            StatusCode::BAD_REQUEST,
            "Connection header did not include 'upgrade'",
        ),
        (
            "upgrade",
            StatusCode::BAD_REQUEST,
            "`Upgrade` header did not include 'websocket'",
        ),
        (
            "sec-websocket-version",
            StatusCode::BAD_REQUEST,
            "`Sec-WebSocket-Version` header did not include '13'",
        ),
        (
            "sec-websocket-key",
            StatusCode::BAD_REQUEST,
            "`Sec-WebSocket-Key` header missing",
        ),
    ] {
        let headers: Vec<_> = VALID.into_iter().filter(|(n, _)| *n != missing).collect();
        let Err(response) = WebSocketUpgrade::from_request(&mut request(&headers)) else {
            panic!("request without {missing} upgraded");
        };
        assert_eq!(response.status(), status, "{missing}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            collect(response.into_body()).await,
            message.as_bytes(),
            "{missing}"
        );
    }

    let mut post = request(&VALID);
    *post.method_mut() = Method::POST;
    let Err(response) = WebSocketUpgrade::from_request(&mut post) else {
        panic!("POST upgraded");
    };
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

    let mut request = request(&VALID);
    request.extensions_mut().clear();
    let Err(response) = WebSocketUpgrade::from_request(&mut request) else {
        panic!("request without upgrade upgraded");
    };
    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
}

#[test]
fn header_tokens_are_found_in_all_values() {
    let mut headers = HeaderMap::new();
    headers.append(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.append(header::CONNECTION, HeaderValue::from_static("TE, Upgrade"));
    headers.append(header::UPGRADE, HeaderValue::from_static("h2c, WebSocket"));
    assert!(header_has_token(&headers, header::CONNECTION, "upgrade"));
    assert!(header_has_token(&headers, header::UPGRADE, "websocket"));
    assert!(!header_has_token(&headers, header::UPGRADE, "socket"));
    assert!(!header_has_token(&headers, header::CONNECTION, "close"));

    headers.append(header::SEC_WEBSOCKET_VERSION, HeaderValue::from_static("8"));
    headers.append(
        header::SEC_WEBSOCKET_VERSION,
        HeaderValue::from_static("13"),
    );
    assert!(header_eq(&headers, header::SEC_WEBSOCKET_VERSION, "13"));
    assert!(!header_eq(&headers, header::SEC_WEBSOCKET_VERSION, "1"));
}

#[tokio::test(start_paused = true)]
async fn hanging_writers_get_aborted() {
    let (alive_tx, alive_rx) = tokio::sync::oneshot::channel::<()>();
    let writer = tokio::spawn(async move {
        let _alive = alive_tx;
        std::future::pending::<()>().await;
    });
    assert_eq!(join_or_abort(writer).await, None);
    // The task got dropped with its sender.
    assert!(alive_rx.await.is_err());
}

#[tokio::test(start_paused = true)]
async fn closes_which_did_not_go_out_get_no_answer() {
    let mut messages = futures::stream::pending::<Result<Message, Infallible>>();
    // A writer which hangs, which does not send its close.
    let start = time::Instant::now();
    finish(
        tokio::spawn(std::future::pending::<Option<()>>()),
        &mut messages,
    )
    .await;
    assert_eq!(start.elapsed(), CLOSE_TIMEOUT);
    // One which did not get its close out.
    let start = time::Instant::now();
    finish(tokio::spawn(async { None::<()> }), &mut messages).await;
    assert_eq!(start.elapsed(), Duration::ZERO);

    // A close which went out gets its answer, what comes before it gets
    // dropped.
    let start = time::Instant::now();
    finish(tokio::spawn(async { Some(()) }), &mut messages).await;
    assert_eq!(start.elapsed(), CLOSE_TIMEOUT);
    let mut messages = futures::stream::iter([
        Ok::<_, Infallible>(Message::Binary(Bytes::from_static(b"late"))),
        Ok(close(false, "")),
    ])
    .chain(futures::stream::pending());
    let start = time::Instant::now();
    finish(tokio::spawn(async { Some(()) }), &mut messages).await;
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test]
async fn bodies_are_sent_at_once() {
    assert!(status(StatusCode::NOT_FOUND).body().is_end_stream());
    let response = text(StatusCode::BAD_REQUEST, "bad");
    assert_eq!(response.body().size_hint().exact(), Some(3));
    assert_eq!(collect(response.into_body()).await, b"bad");
}

#[tokio::test(start_paused = true)]
async fn drains_have_a_budget_of_their_own() -> anyhow::Result<()> {
    let (server, mut client) = tokio::io::duplex(1024);
    let mut socket =
        WebSocketStream::from_raw_socket(ClientIo::new(server), Role::Server, None).await;
    // Dropping the input while the close went out used up the budget
    // almost.
    let discard = socket.get_ref().discarder();
    discard.discard();
    socket.get_mut().left = 10;
    let (sink, messages) = socket.split();

    // The client still sends after the close, more than is left of the
    // first budget, then ends its side.
    let client = tokio::spawn(async move {
        client.write_all(&[0; 4096]).await?;
        time::sleep(Duration::from_millis(50)).await;
        client.write_all(&[0; 4096]).await?;
        client.shutdown().await?;
        // The server ends its side once it read everything.
        let mut rest = vec![];
        client.read_to_end(&mut rest).await?;
        io::Result::Ok(())
    });
    drain(sink, messages).await;
    client.await??;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn dropped_input_wakes_waiting_writes() -> anyhow::Result<()> {
    // A write which waits for the client gets woken once its input gets
    // dropped, which lets the client go on and read.
    let (server, mut client) = tokio::io::duplex(1024);
    let mut io = ClientIo::new(server);
    let discard = io.discarder();
    let writer = tokio::spawn(async move {
        io.write_all(&[1; 64 * 1024]).await?;
        io.flush().await
    });
    time::sleep(Duration::from_millis(50)).await;
    assert!(!writer.is_finished());

    // The client writes before it reads, which the server does not read.
    let client = tokio::spawn(async move {
        client.write_all(&[0; 64 * 1024]).await?;
        let mut read = vec![0; 64 * 1024];
        client.read_exact(&mut read).await?;
        io::Result::Ok(read)
    });
    time::sleep(Duration::from_millis(50)).await;
    assert!(!client.is_finished());

    discard.discard();
    time::timeout(Duration::from_secs(5), writer).await???;
    let read = time::timeout(Duration::from_secs(5), client).await???;
    assert!(read.iter().all(|b| *b == 1));
    Ok(())
}
