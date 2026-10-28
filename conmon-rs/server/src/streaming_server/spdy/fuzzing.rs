pub use super::frame::fuzzing::{frames, header_block};
use super::{
    Connection, Headers,
    frame::{Encoder, Frame},
};
use anyhow::bail;
use bytes::{Bytes, BytesMut};
use tokio::{
    io::AsyncWriteExt,
    runtime, task,
    time::{self, Duration},
};

/// The frames a client sends, by the input: each starts with a byte
/// which picks the frame and its FIN flag, followed by the stream and
/// what the frame needs. The headers of a new stream come from
/// `headers`, by the next bytes of the input. Raw bytes may break the
/// framing.
pub(crate) fn client_frames(
    input: &[u8],
    mut headers: impl FnMut(&mut dyn FnMut() -> u8) -> Headers,
) -> BytesMut {
    let mut input = input.iter().copied();
    let mut encoder = Encoder::default();
    let mut buf = BytesMut::new();
    while let Some(op) = input.next() {
        let mut next = || input.next().unwrap_or_default();
        let (fin, id) = (op & 0x80 != 0, u32::from(next()));
        let frame = match op & 0x7f {
            0 => Frame::SynStream {
                stream_id: id,
                fin,
                headers: headers(&mut next),
            },
            1 => {
                let (byte, len) = (next(), next());
                Frame::Data {
                    stream_id: id,
                    fin,
                    data: Bytes::from(vec![byte; usize::from(len)]),
                }
            }
            2 => Frame::RstStream {
                stream_id: id,
                status: u32::from(next()),
            },
            3 => Frame::Ping { id },
            4 => Frame::GoAway {
                last_stream_id: id,
                status: 0,
            },
            5 => Frame::Headers { stream_id: id, fin },
            6 => Frame::SynReply {
                stream_id: id,
                headers: Headers::default(),
            },
            _ => {
                buf.extend((0..id).map(|_| next()));
                continue;
            }
        };
        // The encoder takes every frame it builds.
        let _ = encoder.encode(&frame, &mut buf);
    }
    buf
}

/// Serve a connection to a client which sends the frames the input
/// describes, then closes the connection. The streams the server
/// accepts take their data, answer, close or get dropped. The connection
/// has to end with the client, also with streams which do not.
pub fn connection(input: &[u8]) {
    let frames = client_frames(input, |next| {
        [(
            "streamType",
            ["data", "error", "other"][usize::from(next() % 3)],
        )]
        .into()
    });
    let runtime = runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (mut from_server, mut to_server) = tokio::io::split(client);
        let mut connection = Connection::serve(server, None, None, |headers| {
            match headers.get("streamType") {
                Some("data" | "error") => Ok(()),
                _ => bail!("unexpected stream type"),
            }
        });
        // The client reads while it writes, a server which answers more
        // than the pipe holds would wait for it otherwise.
        let client = task::spawn(async move {
            let write = async {
                let _ = to_server.write_all(&frames).await;
                let _ = to_server.shutdown().await;
            };
            let mut sink = tokio::io::sink();
            let _ = tokio::join!(write, tokio::io::copy(&mut from_server, &mut sink));
        });
        let served = async {
            let mut streams = vec![];
            while let Some(stream) = connection.accept().await {
                streams.push(task::spawn(async move {
                    let id = stream.id();
                    let (mut receiver, mut sender) = stream.split();
                    match id % 3 {
                        0 => {}
                        1 => {
                            while receiver.recv().await.is_some() {}
                            let _ = sender.send(Bytes::from_static(b"done")).await;
                            let _ = sender.close().await;
                        }
                        _ => {
                            let _ = sender.send(Bytes::from_static(b"hello")).await;
                            let _ = sender.close().await;
                            while receiver.recv_held().await.is_some() {}
                        }
                    }
                }));
            }
            for stream in streams {
                let _ = stream.await;
            }
            let _ = client.await;
        };
        time::timeout(Duration::from_secs(30), served)
            .await
            .expect("the connection ended with the client");
    });
}
