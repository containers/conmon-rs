use super::*;

/// Check the headers of an upgrade request, one `name: value` per line
/// of the input, like the server does for a WebSocket or SPDY. Lines
/// which are no valid header get skipped. A GET request, so that the
/// WebSocket upgrade checks all of its headers, it rejects other methods
/// right away.
pub fn upgrade_headers(input: &[u8]) {
    let mut request = Request::new(());
    let headers = request.headers_mut();
    for line in input.split(|b| *b == b'\n') {
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            continue;
        };
        let (name, value) = (&line[..colon], line[colon + 1..].trim_ascii());
        if let (Ok(name), Ok(value)) =
            (HeaderName::from_bytes(name), HeaderValue::from_bytes(value))
        {
            headers.append(name, value);
        }
    }

    if let Ok((version, protocol)) =
        StreamingServer::spdy_handshake(request.headers(), &SPDY_PROTOCOLS)
    {
        assert!(SPDY_PROTOCOLS.contains(&(protocol, version)));
    }
    let _ = StreamingServer::spdy_handshake(request.headers(), &[(port_forward::PROTOCOL, ())]);
    let requested = StreamingServer::websocket_protocols(request.headers());
    let _ = Framing::negotiate(requested.iter().map(String::as_str));
    let _ = WebSocketProtocol::negotiate(requested.iter().map(String::as_str));
    let _ = StreamingServer::is_websocket_request(request.headers());
    // Without a connection to upgrade, no request passes.
    assert!(WebSocketUpgrade::from_request(&mut request).is_err());
}
