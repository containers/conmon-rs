use super::{
    MAX_CHANNEL_PORTS, PORT_HEADER, PortForward, REQUEST_ID_HEADER, STREAM_TYPE_DATA,
    STREAM_TYPE_ERROR,
};
use crate::{
    mutex::lock,
    streaming_server::{
        spdy::fuzzing::client_frames,
        stream_protocol::{STREAM_CREATION_TIMEOUT, STREAM_TYPE_HEADER},
    },
};
use std::{
    pin::{Pin, pin},
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::{
    io::{self, AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
    runtime, select, task,
    time::{self, Duration, Instant},
};

/// How long the server may go without reading the client before the
/// session counts as hung. A stream which does not take its data stops
/// reading the client until its request expires, at most
/// [`STREAM_CREATION_TIMEOUT`] after the read which created it, and the
/// client waits a little longer than that before it closes the
/// connection.
const MAX_STALL: Duration = STREAM_CREATION_TIMEOUT.saturating_mul(2);

/// The server side of the connection, which notes when the server read
/// from the client the last time.
struct Watched {
    io: DuplexStream,
    last_read: Arc<Mutex<Instant>>,
}

impl AsyncRead for Watched {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.io).poll_read(cx, buf);
        if poll.is_ready() {
            *lock(&this.last_read) = Instant::now();
        }
        poll
    }
}

impl AsyncWrite for Watched {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// Parse the query of a channel protocol request, the ports it gives
/// are the ones a session can forward.
pub fn query_ports(query: &str) {
    if let Ok(ports) = PortForward::query_ports(Some(query)) {
        assert!(!ports.is_empty());
        assert!(ports.len() <= MAX_CHANNEL_PORTS);
        assert!(!ports.contains(&0));
    }
}

/// Serve a SPDY port forward session to a client which sends the frames
/// the input describes, like the SPDY connection target, then closes the
/// connection. The streams pair up into requests, which fail to enter
/// the network namespace, so that only the pairing, the resets and the
/// errors run. The session has to end with the client. The namespace
/// thread answers in real time while the clock is paused, so whether an
/// error reaches the client before it closes varies between runs.
///
/// The server may stop reading the client while a stream waits for the
/// other one of its request, until the request expires. The client
/// waits for the server meanwhile, and a request which arrives after
/// one expired may stop the server again. So the session as a whole
/// may take several request timeouts, but the server must never go
/// `MAX_STALL` without reading the client until it ended.
pub fn spdy(input: &[u8]) {
    let frames = client_frames(input, |next| {
        // Mostly valid streams, so that requests pair up.
        let typ = match next() % 8 {
            0..4 => STREAM_TYPE_DATA,
            4..7 => STREAM_TYPE_ERROR,
            _ => "other",
        };
        let port = match next() % 8 {
            0..6 => "80",
            6 => "0",
            _ => "http",
        };
        // Old clients send no request ID, their streams pair by their
        // IDs.
        let id = next();
        let id = if id < 64 {
            String::new()
        } else {
            (id % 4).to_string()
        };
        [
            (STREAM_TYPE_HEADER, typ),
            (PORT_HEADER, port),
            (REQUEST_ID_HEADER, id.as_str()),
        ]
        .into()
    });
    // The paused clock gets to the deadlines of the requests at once.
    let runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (client, server) = io::duplex(64 * 1024);
        let (mut from_server, mut to_server) = io::split(client);
        let client = task::spawn(async move {
            // The connection stays open until the requests whose second
            // stream did not come expired, the streams of a connection
            // which ended count as reset.
            let write = async {
                let _ = to_server.write_all(&frames).await;
                time::sleep(STREAM_CREATION_TIMEOUT + Duration::from_secs(1)).await;
                let _ = to_server.shutdown().await;
            };
            let mut sink = io::sink();
            let _ = tokio::join!(write, io::copy(&mut from_server, &mut sink));
        });
        let session = PortForward {
            netns_path: "/proc/self/ns/none".into(),
            idle_timeout: None,
            ping_interval: None,
        };
        let last_read = Arc::new(Mutex::new(Instant::now()));
        let server = Watched {
            io: server,
            last_read: last_read.clone(),
        };
        let mut served = pin!(async {
            session.serve_spdy(server).await;
            client.await.expect("client task");
        });
        let last_read = || *lock(&last_read);
        loop {
            select! {
                biased;
                () = &mut served => break,
                () = time::sleep_until(last_read() + MAX_STALL) => assert!(
                    last_read().elapsed() < MAX_STALL,
                    "the session ended with the client"
                ),
            }
        }
    });
}
