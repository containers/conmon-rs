use super::{
    CLOSE_BYTE, ClientEvent, ClientInput, ExpectedStreams, Framing, Message, Output, RESIZE_BYTE,
    SPDY_PROTOCOLS, STDIN_BYTE, SpdyStreams, Status, Stdin,
    spdy::{ResizeEvents, STREAM_TYPE_RESIZE},
    websocket::WEBSOCKET_PROTOCOLS,
};
use crate::streaming_server::{
    spdy::{Connection, Headers, fuzzing::client_frames},
    stream_protocol::{STREAM_CREATION_TIMEOUT, STREAM_TYPE_HEADER},
};
use bytes::Bytes;
use serde::de::IgnoredAny;
use tokio::{
    io::{self, AsyncWriteExt},
    runtime, task,
    time::{self, Duration},
};

/// The resize stream of a SPDY client, split into parts at NUL bytes.
/// Every event is a single JSON value, without whitespace around it.
pub fn resize_events(data: &[u8]) {
    let mut events = ResizeEvents::default();
    for part in data.split(|b| *b == 0) {
        events.push(part);
        while let Some(event) = events.pop() {
            assert!(!event.is_empty() && event.trim_ascii().len() == event.len());
            assert!(serde_json::from_slice::<IgnoredAny>(&event).is_ok());
        }
    }
}

/// The messages of a WebSocket client, for the framing the first byte
/// picks. Each message is a byte for its type, a byte for its length and
/// its payload. The events of each go to `event`, binary ones get checked
/// against the framing.
pub fn messages(input: &[u8], mut event: impl FnMut(ClientEvent)) {
    let Some((&framing, mut input)) = input.split_first() else {
        return;
    };
    let framing = WEBSOCKET_PROTOCOLS[usize::from(framing) % WEBSOCKET_PROTOCOLS.len()].1;
    while let [typ, len, rest @ ..] = input {
        let (payload, next) = rest.split_at(usize::from(*len).min(rest.len()));
        input = next;
        let data = Bytes::copy_from_slice(payload);
        let message = match typ % 5 {
            0 => Message::Binary(data.clone()),
            1 => Message::Text(String::from_utf8_lossy(payload).into_owned().into()),
            2 => Message::Close(None),
            3 => Message::Ping(data.clone()),
            _ => Message::Pong(data.clone()),
        };
        let binary = matches!(message, Message::Binary(_));
        let Some(got) = framing.event(message) else {
            continue;
        };
        if binary && framing == Framing::V5 {
            let expected = match payload {
                [STDIN_BYTE, stdin @ ..] => Some(ClientEvent::Stdin(Stdin::Data(
                    Bytes::copy_from_slice(stdin),
                ))),
                [CLOSE_BYTE, STDIN_BYTE] => Some(ClientEvent::Stdin(Stdin::Close)),
                [RESIZE_BYTE, resize @ ..] => {
                    Some(ClientEvent::Resize(Bytes::copy_from_slice(resize)))
                }
                _ => None,
            };
            assert_eq!(Some(&got), expected.as_ref());
        }
        if let ClientEvent::Stdin(Stdin::Data(stdin)) = &got {
            assert!(!stdin.is_empty());
        }
        event(got);
    }
}

/// Serve an exec or attach session over SPDY to a client which sends the
/// frames the input describes, like the SPDY connection target, then
/// closes the connection. The first byte picks the version and the
/// streams the session expects. The streams of the client have random
/// types, which may come twice, get reset or come after the start. The
/// session has to start or fail, and end with the client: the server may
/// stop reading the client until the streams came, at most
/// [`STREAM_CREATION_TIMEOUT`], the client waits a little longer than
/// that before it closes the connection.
pub fn spdy(input: &[u8]) {
    let Some((&first, input)) = input.split_first() else {
        return;
    };
    let version = SPDY_PROTOCOLS[usize::from(first) % SPDY_PROTOCOLS.len()].1;
    let expected = ExpectedStreams {
        stdin: first & 0x10 != 0,
        stdout: first & 0x20 != 0,
        stderr: first & 0x40 != 0,
    };
    let frames = client_frames(input, |next| {
        let typ = match next() % 8 {
            0 | 1 => "error",
            2 => "stdin",
            3 => "stdout",
            4 => "stderr",
            5 => STREAM_TYPE_RESIZE,
            6 => "other",
            _ => return Headers::default(),
        };
        [(STREAM_TYPE_HEADER, typ)].into()
    });
    // The paused clock gets to the timeouts at once.
    let runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (client, server) = io::duplex(64 * 1024);
        let (mut from_server, mut to_server) = io::split(client);
        let client = task::spawn(async move {
            let write = async {
                let _ = to_server.write_all(&frames).await;
                time::sleep(STREAM_CREATION_TIMEOUT + Duration::from_secs(1)).await;
                let _ = to_server.shutdown().await;
            };
            let mut sink = io::sink();
            let _ = tokio::join!(write, io::copy(&mut from_server, &mut sink));
        });
        let served = async {
            let mut connection = Connection::serve(server, None, None, move |headers| {
                SpdyStreams::accept(headers, version)
            });
            match SpdyStreams::wait(&mut connection, expected).await {
                Ok(streams) => {
                    let (mut input, output) = streams.into_parts(connection, version).await;
                    while let Some(event) = input.next().await {
                        match event {
                            ClientEvent::Stdin(Stdin::Data(data)) => assert!(!data.is_empty()),
                            ClientEvent::Stdin(Stdin::Close) => {}
                            ClientEvent::Resize(event) => {
                                assert!(serde_json::from_slice::<IgnoredAny>(&event).is_ok());
                            }
                            ClientEvent::Closed => break,
                            ClientEvent::TooLarge => panic!("SPDY has no message size limit"),
                        }
                    }
                    let mut output = Output::spdy(output, None);
                    let _ = output.status(&Status::Success).await;
                    output.close().await;
                }
                Err(_) => connection.handle().abort(),
            }
            client.await.expect("client task");
        };
        time::timeout(4 * STREAM_CREATION_TIMEOUT, served)
            .await
            .expect("the session ended with the client");
    });
}
