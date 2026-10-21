//! A SPDY/3.1 server for the streaming protocols of Kubernetes. The kubelet
//! still talks SPDY to the streaming server of a container runtime, also when
//! the client used a WebSocket, which the API server or the kubelet unwrap.
//!
//! Only what moby/spdystream, the SPDY implementation of Kubernetes, uses is
//! supported: the client opens all streams, the server answers pings. There
//! is no flow control, spdystream ignores window updates, so the server
//! bounds what it buffers itself and stops reading the connection when a
//! stream does not take its data. Streams have to take their data while they
//! wait for something else, otherwise they hold up all others.

mod frame;

use crate::idle::Activity;
use anyhow::{Context, Result, anyhow, bail};
use bytes::{Bytes, BytesMut};
pub use frame::Headers;
use frame::{
    Decoder, Encoder, Frame, GOAWAY_OK, GOAWAY_PROTOCOL_ERROR, STATUS_CANCEL,
    STATUS_PROTOCOL_ERROR, STATUS_REFUSED_STREAM,
};
use std::{
    collections::HashMap,
    mem,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
    select,
    sync::{Notify, mpsc},
    time::{self, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

/// How many control frames wait for the connection.
const CONTROL_QUEUE: usize = 64;

/// How many data frames of all streams wait for the connection.
const DATA_QUEUE: usize = 16;

/// How many data frames of a stream wait for the stream to take them, before
/// the connection is not read anymore.
const STREAM_QUEUE: usize = 16;

/// How many accepted streams wait for [`Connection::accept`].
const ACCEPT_QUEUE: usize = 16;

/// The most streams a client may have open at once. A Kubernetes port forward
/// uses two per forwarded connection.
const MAX_STREAMS: usize = 1024;

/// How many bytes the writer collects before writing them to the connection.
const WRITE_BUFFER: usize = 64 * 1024;

/// How long the last frames may take to go out once the connection ends.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection a client upgraded to SPDY/3.1.
pub struct Connection {
    streams: mpsc::Receiver<Stream>,
}

impl Connection {
    /// Serve SPDY on the connection. The client opens streams, `accept`
    /// decides about each by its headers, rejected ones get reset. The
    /// connection ends when the client closes it, violates the protocol, or
    /// when no frame went either way for the idle timeout.
    pub fn serve<T, F>(io: T, idle_timeout: Option<Duration>, accept: F) -> Self
    where
        T: AsyncRead + AsyncWrite + Send + 'static,
        F: Fn(&Headers) -> Result<()> + Send + 'static,
    {
        let encoder = Encoder::default();
        let (reader, writer) = tokio::io::split(io);
        let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE);
        let (data_tx, data_rx) = mpsc::channel(DATA_QUEUE);
        let (streams_tx, streams_rx) = mpsc::channel(ACCEPT_QUEUE);

        let shared = Arc::new(Shared {
            control: control_tx,
            data: data_tx,
            streams: Mutex::default(),
            resets: Mutex::default(),
            resets_ready: Notify::new(),
            done: CancellationToken::new(),
            activity: Activity::new(),
            last_stream_id: AtomicU32::new(0),
        });
        tokio::spawn(
            Reader {
                shared: shared.clone(),
                reader,
                accept,
                streams: streams_tx,
            }
            .run(),
        );
        // On its own, the reader may wait for a stream which does not take
        // its data.
        tokio::spawn(Self::watch_idle(shared.clone(), idle_timeout));
        tokio::spawn(Self::write(shared, writer, encoder, control_rx, data_rx));

        Self {
            streams: streams_rx,
        }
    }

    /// The next stream the client opened, `None` once the connection ended.
    pub async fn accept(&mut self) -> Option<Stream> {
        self.streams.recv().await
    }

    /// End the connection once no frame went either way for the timeout.
    async fn watch_idle(shared: Arc<Shared>, timeout: Option<Duration>) {
        select! {
            _ = shared.activity.idle(timeout) => {
                debug!("Closing idle SPDY connection");
                shared.go_away(GOAWAY_OK);
            }
            _ = shared.done.cancelled() => {}
        }
    }

    /// Write the queued frames: control frames first, then the resets of the
    /// streams, then data. The data frames of a stream keep their order, its
    /// FIN goes through the same queue. A reset follows the SYN_REPLY of its
    /// stream, which is queued before the stream exists.
    async fn write<T>(
        shared: Arc<Shared>,
        mut writer: WriteHalf<T>,
        mut encoder: Encoder,
        mut control_rx: mpsc::Receiver<Frame>,
        mut data_rx: mpsc::Receiver<Frame>,
    ) where
        T: AsyncWrite,
    {
        let mut buf = BytesMut::with_capacity(WRITE_BUFFER);
        let result = async {
            loop {
                let data = select! {
                    biased;
                    _ = shared.done.cancelled() => break,
                    Some(frame) = control_rx.recv() => {
                        encoder.encode(&frame, &mut buf)?;
                        None
                    }
                    _ = shared.resets_ready.notified() => None,
                    Some(frame) = data_rx.recv() => Some(frame),
                    else => break,
                };
                // Whatever else is queued already goes out with it.
                while let Ok(frame) = control_rx.try_recv() {
                    encoder.encode(&frame, &mut buf)?;
                }
                shared.encode_resets(&mut encoder, &mut buf)?;
                if let Some(frame) = data {
                    encoder.encode(&frame, &mut buf)?;
                }
                while buf.len() < WRITE_BUFFER {
                    let Ok(frame) = data_rx.try_recv() else {
                        break;
                    };
                    encoder.encode(&frame, &mut buf)?;
                }
                if buf.is_empty() {
                    continue;
                }
                select! {
                    biased;
                    result = writer.write_all(&buf) => result.context("write frames")?,
                    // A client which does not read anymore must not keep the
                    // connection open.
                    _ = shared.done.cancelled() => return Ok(()),
                }
                buf.clear();
                shared.activity.touch();
            }

            // The reader queues a GOAWAY or resets before it ends the
            // connection.
            while let Ok(frame) = control_rx.try_recv() {
                encoder.encode(&frame, &mut buf)?;
            }
            shared.encode_resets(&mut encoder, &mut buf)?;
            time::timeout(SHUTDOWN_TIMEOUT, async {
                writer.write_all(&buf).await.context("write frames")?;
                writer.shutdown().await.context("shut down connection")
            })
            .await
            .context("shut down connection")?
        }
        .await;

        if let Err(e) = result {
            debug!("SPDY connection ended: {e:#}");
        }
        shared.done.cancel();
    }
}

/// What the reader, the writer and the streams of a connection share.
struct Shared {
    control: mpsc::Sender<Frame>,
    data: mpsc::Sender<Frame>,
    streams: Mutex<HashMap<u32, StreamState>>,

    /// The streams the server reset, which the writer sends next. Unlike the
    /// control queue it cannot be full, a stream gets reset only once.
    resets: Mutex<Vec<u32>>,
    resets_ready: Notify,

    /// Cancelled once the connection ended.
    done: CancellationToken,

    /// When the last frame got read or written, for the idle timeout.
    activity: Activity,

    /// The last stream the client opened, for the GOAWAY.
    last_stream_id: AtomicU32,
}

/// An open stream as the reader sees it.
struct StreamState {
    /// The data of the client, `None` once the client closed its side.
    data: Option<mpsc::Sender<Bytes>>,

    /// Cancelled when the stream got reset or the connection ended.
    reset: CancellationToken,

    /// Whether the server closed its side.
    local_closed: bool,
}

impl Shared {
    fn lock_streams(&self) -> std::sync::MutexGuard<'_, HashMap<u32, StreamState>> {
        self.streams.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue the reset of a stream for the writer.
    fn queue_reset(&self, id: u32) {
        self.resets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id);
        self.resets_ready.notify_one();
    }

    fn encode_resets(&self, encoder: &mut Encoder, buf: &mut BytesMut) -> Result<()> {
        let resets = mem::take(&mut *self.resets.lock().unwrap_or_else(|e| e.into_inner()));
        for stream_id in resets {
            encoder.encode(
                &Frame::RstStream {
                    stream_id,
                    status: STATUS_CANCEL,
                },
                buf,
            )?;
        }
        Ok(())
    }

    /// The client closed its side of the stream.
    fn remote_closed(&self, id: u32) {
        let mut streams = self.lock_streams();
        if let Some(state) = streams.get_mut(&id) {
            state.data = None;
            if state.local_closed {
                streams.remove(&id);
            }
        }
    }

    /// The server closed its side of the stream.
    fn local_closed(&self, id: u32) {
        let mut streams = self.lock_streams();
        if let Some(state) = streams.get_mut(&id) {
            state.local_closed = true;
            if state.data.is_none() {
                streams.remove(&id);
            }
        }
    }

    /// Hand the data of the client to its stream. Waits while the stream
    /// does not take it, which stops reading the connection.
    async fn deliver(&self, stream_id: u32, data: Bytes) {
        let Some((tx, reset)) = self
            .lock_streams()
            .get(&stream_id)
            .and_then(|s| Some((s.data.clone()?, s.reset.clone())))
        else {
            trace!("Dropping data for closed SPDY stream {stream_id}");
            return;
        };
        if data.is_empty() {
            return;
        }
        select! {
            // A stream which went away drops its data.
            _ = tx.send(data) => {}
            _ = reset.cancelled() => {}
        }
    }

    async fn send_control(&self, frame: Frame) -> Result<()> {
        select! {
            result = self.control.send(frame) => result.map_err(|_| anyhow!("connection closed")),
            _ = self.done.cancelled() => bail!("connection closed"),
        }
    }

    /// End the connection with a GOAWAY, which the writer sends on its way
    /// out unless the client does not read anymore.
    fn go_away(&self, status: u32) {
        let _ = self.control.try_send(Frame::GoAway {
            last_stream_id: self.last_stream_id.load(Ordering::Relaxed),
            status,
        });
        self.done.cancel();
    }

    /// Forget a stream which got reset by either side.
    fn remove(&self, id: u32) {
        if let Some(state) = self.lock_streams().remove(&id) {
            state.reset.cancel();
        }
    }
}

/// Reads the frames of the client and dispatches them to the streams.
struct Reader<T, F> {
    shared: Arc<Shared>,
    reader: ReadHalf<T>,
    accept: F,
    streams: mpsc::Sender<Stream>,
}

/// Why the reader stopped.
enum End {
    /// The client closed the connection, nothing takes streams anymore or
    /// the connection ended otherwise, like when it was idle.
    Closed,

    /// The client violated the protocol, the connection cannot continue.
    ProtocolError(anyhow::Error),
}

impl<T, F> Reader<T, F>
where
    T: AsyncRead,
    F: Fn(&Headers) -> Result<()>,
{
    async fn run(mut self) {
        let shared = self.shared.clone();
        if let End::ProtocolError(e) = self.read().await {
            warn!("Closing SPDY connection: {e:#}");
            shared.go_away(GOAWAY_PROTOCOL_ERROR);
        }

        shared.done.cancel();
        // Ends the data of all streams, the reset tokens are children of
        // `done` and got cancelled with it.
        shared.lock_streams().clear();
    }

    async fn read(&mut self) -> End {
        let mut decoder = Decoder::default();
        let mut going_away = false;

        loop {
            let frame = select! {
                frame = decoder.read(&mut self.reader) => frame,
                _ = self.shared.done.cancelled() => return End::Closed,
            };
            let frame = match frame {
                Ok(Some(frame)) => frame,
                Ok(None) => return End::Closed,
                Err(e) => return End::ProtocolError(e),
            };
            self.shared.activity.touch();
            trace!("Got SPDY frame {frame:?}");

            let result = match frame {
                Frame::SynStream {
                    stream_id,
                    fin,
                    headers,
                } => {
                    // Client stream IDs are odd and increase.
                    let last_stream_id = self.shared.last_stream_id.load(Ordering::Relaxed);
                    if stream_id % 2 == 0 || stream_id <= last_stream_id {
                        self.shared
                            .send_control(Frame::RstStream {
                                stream_id,
                                status: STATUS_PROTOCOL_ERROR,
                            })
                            .await
                    } else {
                        self.shared
                            .last_stream_id
                            .store(stream_id, Ordering::Relaxed);
                        self.open(stream_id, fin, headers, going_away).await
                    }
                }
                Frame::Data {
                    stream_id,
                    fin,
                    data,
                } => {
                    self.shared.deliver(stream_id, data).await;
                    if fin {
                        self.shared.remote_closed(stream_id);
                    }
                    Ok(())
                }
                Frame::Headers { stream_id, fin } => {
                    if fin {
                        self.shared.remote_closed(stream_id);
                    }
                    Ok(())
                }
                Frame::RstStream { stream_id, status } => {
                    debug!("Client reset SPDY stream {stream_id} with status {status}");
                    self.shared.remove(stream_id);
                    Ok(())
                }
                // Pings of the client have odd IDs, the server sends none.
                Frame::Ping { id } if id % 2 == 1 => {
                    self.shared.send_control(Frame::Ping { id }).await
                }
                Frame::GoAway { .. } => {
                    debug!("Client is going away");
                    going_away = true;
                    Ok(())
                }
                Frame::Ping { .. } | Frame::SynReply { .. } | Frame::Ignored => Ok(()),
            };
            if result.is_err() {
                return End::Closed;
            }
        }
    }

    /// Accept or refuse a stream the client opened.
    async fn open(
        &mut self,
        stream_id: u32,
        fin: bool,
        headers: Headers,
        going_away: bool,
    ) -> Result<()> {
        let refused = if going_away {
            Some(STATUS_REFUSED_STREAM)
        } else if self.shared.lock_streams().len() >= MAX_STREAMS {
            warn!("Refusing SPDY stream {stream_id}, too many streams are open");
            Some(STATUS_REFUSED_STREAM)
        } else if let Err(e) = (self.accept)(&headers) {
            warn!("Rejecting SPDY stream {stream_id}: {e:#}");
            Some(STATUS_CANCEL)
        } else {
            None
        };
        if let Some(status) = refused {
            return self
                .shared
                .send_control(Frame::RstStream { stream_id, status })
                .await;
        }

        let reset = self.shared.done.child_token();
        let (data_tx, data_rx) = mpsc::channel(STREAM_QUEUE);
        self.shared.lock_streams().insert(
            stream_id,
            StreamState {
                data: (!fin).then_some(data_tx),
                reset: reset.clone(),
                local_closed: false,
            },
        );
        // Queued before anything the stream could send.
        self.shared
            .send_control(Frame::SynReply {
                stream_id,
                headers: Headers::default(),
            })
            .await?;

        let stream = Stream {
            headers,
            receiver: StreamReceiver {
                data: data_rx,
                reset: reset.clone(),
            },
            sender: StreamSender {
                id: stream_id,
                shared: self.shared.clone(),
                reset,
                closed: false,
            },
        };
        select! {
            result = self.streams.send(stream) => {
                result.map_err(|_| anyhow!("streams are not accepted anymore"))
            }
            _ = self.shared.done.cancelled() => bail!("connection closed"),
        }
    }
}

/// A stream the client opened and the server accepted.
pub struct Stream {
    headers: Headers,
    receiver: StreamReceiver,
    sender: StreamSender,
}

impl Stream {
    /// The headers the client opened the stream with.
    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    /// The ID of the stream.
    pub fn id(&self) -> u32 {
        self.sender.id
    }

    /// Split the stream into the data of the client and the sending side,
    /// to use both at the same time.
    pub fn split(self) -> (StreamReceiver, StreamSender) {
        (self.receiver, self.sender)
    }

    /// Send data, see [`StreamSender::send`].
    pub async fn send(&mut self, data: Bytes) -> Result<()> {
        self.sender.send(data).await
    }

    /// Close the sending side, see [`StreamSender::close`].
    pub async fn close(&mut self) -> Result<()> {
        self.sender.close().await
    }
}

/// The data the client sends on a stream.
pub struct StreamReceiver {
    data: mpsc::Receiver<Bytes>,
    reset: CancellationToken,
}

impl StreamReceiver {
    /// The next data, `None` once the client closed its side, the stream got
    /// reset or the connection ended.
    pub async fn recv(&mut self) -> Option<Bytes> {
        self.data.recv().await
    }

    /// Whether the stream got reset or the connection ended.
    pub fn is_reset(&self) -> bool {
        self.reset.is_cancelled()
    }
}

/// The sending side of a stream. Dropping it without closing it resets the
/// stream.
pub struct StreamSender {
    id: u32,
    shared: Arc<Shared>,
    reset: CancellationToken,
    closed: bool,
}

impl StreamSender {
    /// Send data in a single frame, at most [`frame::MAX_DATA_FRAME_LEN`]
    /// bytes. Fails once the stream got reset or the connection ended.
    pub async fn send(&mut self, data: Bytes) -> Result<()> {
        if self.closed {
            bail!("SPDY stream {} is closed", self.id);
        }
        select! {
            biased;
            _ = self.reset.cancelled() => bail!("SPDY stream {} got reset", self.id),
            result = self.shared.data.send(Frame::Data {
                stream_id: self.id,
                fin: false,
                data,
            }) => result.map_err(|_| anyhow!("SPDY connection closed")),
        }
    }

    /// Close the sending side after the data sent before.
    pub async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let result = select! {
            biased;
            _ = self.reset.cancelled() => Err(anyhow!("SPDY stream {} got reset", self.id)),
            result = self.shared.data.send(Frame::Data {
                stream_id: self.id,
                fin: true,
                data: Bytes::new(),
            }) => result.map_err(|_| anyhow!("SPDY connection closed")),
        };
        self.shared.local_closed(self.id);
        result
    }

    /// Reset the stream, both sides end. The reset may go out before data
    /// queued for the stream, which the client discards then.
    pub fn reset(&mut self) {
        if self.closed && self.reset.is_cancelled() {
            return;
        }
        self.closed = true;
        if !self.reset.is_cancelled() {
            self.shared.queue_reset(self.id);
        }
        self.shared.remove(self.id);
    }

    /// Resolves once the client reset the stream or the connection ended.
    pub async fn reset_by_peer(&self) {
        self.reset.cancelled().await
    }

    /// Whether the stream got reset or the connection ended.
    pub fn is_reset(&self) -> bool {
        self.reset.is_cancelled()
    }
}

impl Drop for StreamSender {
    fn drop(&mut self) {
        if !self.closed {
            self.reset();
        }
    }
}

/// A SPDY client for the tests of the protocols on top.
#[cfg(test)]
pub mod testing {
    use super::frame::{Decoder, Encoder};
    pub use super::frame::{Frame, Headers};
    use anyhow::Result;
    use bytes::BytesMut;
    use tokio::{
        io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, duplex},
        time::{self, Duration},
    };

    /// The client side of a connection.
    pub struct Client<T = DuplexStream> {
        io: T,
        encoder: Encoder,
        decoder: Decoder,
    }

    impl Client {
        /// A client and the server side of its connection.
        pub fn pair() -> Result<(Self, DuplexStream)> {
            let (client, server) = duplex(1024 * 1024);
            Ok((Self::new(client)?, server))
        }
    }

    impl<T> Client<T>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        pub fn new(io: T) -> Result<Self> {
            Ok(Self {
                io,
                encoder: Encoder::default(),
                decoder: Decoder::default(),
            })
        }

        pub fn io(&mut self) -> &mut T {
            &mut self.io
        }

        pub async fn send(&mut self, frame: Frame) -> Result<()> {
            let mut buf = BytesMut::new();
            self.encoder.encode(&frame, &mut buf)?;
            self.io.write_all(&buf).await?;
            Ok(())
        }

        pub async fn open(&mut self, stream_id: u32, headers: Headers) -> Result<()> {
            self.send(Frame::SynStream {
                stream_id,
                fin: false,
                headers,
            })
            .await
        }

        /// The next frame of the server. Long enough for the timeouts of the
        /// tests, which run with paused time.
        pub async fn frame(&mut self) -> Result<Option<Frame>> {
            time::timeout(Duration::from_secs(60), self.decoder.read(&mut self.io)).await?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::Client, *};
    use tokio::io::duplex;

    fn connect(
        accept: fn(&Headers) -> Result<()>,
        idle_timeout: Option<Duration>,
    ) -> Result<(Client, Connection)> {
        let (client, server) = Client::pair()?;
        Ok((client, Connection::serve(server, idle_timeout, accept)))
    }

    fn accept_data(headers: &Headers) -> Result<()> {
        match headers.get("streamType") {
            Some("data") => Ok(()),
            typ => bail!("unexpected stream type {typ:?}"),
        }
    }

    #[tokio::test]
    async fn streams_carry_data_both_ways() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, None)?;
        client.open(1, [("streamtype", "data")].into()).await?;
        let mut stream = connection.accept().await.context("stream")?;
        assert_eq!(stream.id(), 1);
        assert_eq!(stream.headers().get("streamtype"), Some("data"));
        assert_eq!(
            client.frame().await?,
            Some(Frame::SynReply {
                stream_id: 1,
                headers: Headers::default()
            })
        );

        client
            .send(Frame::Data {
                stream_id: 1,
                fin: true,
                data: Bytes::from_static(b"ping"),
            })
            .await?;
        stream.send(Bytes::from_static(b"pong")).await?;
        stream.close().await?;

        let (mut receiver, sender) = stream.split();
        assert_eq!(receiver.recv().await, Some(Bytes::from_static(b"ping")));
        assert_eq!(receiver.recv().await, None);
        assert!(!receiver.is_reset());

        assert_eq!(
            client.frame().await?,
            Some(Frame::Data {
                stream_id: 1,
                fin: false,
                data: Bytes::from_static(b"pong")
            })
        );
        assert_eq!(
            client.frame().await?,
            Some(Frame::Data {
                stream_id: 1,
                fin: true,
                data: Bytes::new()
            })
        );
        // Both sides are closed, the connection forgot the stream.
        assert!(sender.shared.lock_streams().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn rejected_streams_get_reset() -> Result<()> {
        let (mut client, _connection) = connect(accept_data, None)?;
        client.open(1, [("streamtype", "unknown")].into()).await?;
        assert_eq!(
            client.frame().await?,
            Some(Frame::RstStream {
                stream_id: 1,
                status: STATUS_CANCEL
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_stream_ids_get_reset() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, None)?;
        client.open(3, [("streamtype", "data")].into()).await?;
        let _stream = connection.accept().await.context("stream")?;
        client.frame().await?;

        for stream_id in [2, 3, 1] {
            client
                .open(stream_id, [("streamtype", "data")].into())
                .await?;
            assert_eq!(
                client.frame().await?,
                Some(Frame::RstStream {
                    stream_id,
                    status: STATUS_PROTOCOL_ERROR
                })
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn pings_get_echoed() -> Result<()> {
        let (mut client, _connection) = connect(accept_data, None)?;
        client.send(Frame::Ping { id: 2 }).await?;
        client.send(Frame::Ping { id: 1 }).await?;
        // Only the one the client started.
        assert_eq!(client.frame().await?, Some(Frame::Ping { id: 1 }));
        Ok(())
    }

    #[tokio::test]
    async fn client_resets_end_the_stream() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, None)?;
        client.open(1, [("streamtype", "data")].into()).await?;
        let stream = connection.accept().await.context("stream")?;
        client.frame().await?;

        client
            .send(Frame::RstStream {
                stream_id: 1,
                status: STATUS_CANCEL,
            })
            .await?;
        let (mut receiver, mut sender) = stream.split();
        time::timeout(Duration::from_secs(5), sender.reset_by_peer()).await?;
        assert_eq!(receiver.recv().await, None);
        assert!(receiver.is_reset());
        assert!(sender.send(Bytes::from_static(b"late")).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn dropped_senders_reset_the_stream() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, None)?;
        client.open(1, [("streamtype", "data")].into()).await?;
        drop(connection.accept().await.context("stream")?);
        client.frame().await?;
        assert_eq!(
            client.frame().await?,
            Some(Frame::RstStream {
                stream_id: 1,
                status: STATUS_CANCEL
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn closing_the_connection_ends_all_streams() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, None)?;
        client.open(1, [("streamtype", "data")].into()).await?;
        let stream = connection.accept().await.context("stream")?;
        client.frame().await?;

        drop(client);
        let (mut receiver, mut sender) = stream.split();
        time::timeout(Duration::from_secs(5), sender.reset_by_peer()).await?;
        assert_eq!(receiver.recv().await, None);
        assert!(sender.send(Bytes::from_static(b"late")).await.is_err());
        assert!(connection.accept().await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn protocol_errors_end_the_connection() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, None)?;
        client.open(3, [("streamtype", "data")].into()).await?;
        let _stream = connection.accept().await.context("stream")?;
        client.frame().await?;
        // A control frame of SPDY/2.
        client
            .io()
            .write_all(&[0x80, 0x02, 0, 6, 0, 0, 0, 4, 0, 0, 0, 1])
            .await?;
        // The GOAWAY names the last stream of the client.
        assert_eq!(
            client.frame().await?,
            Some(Frame::GoAway {
                last_stream_id: 3,
                status: GOAWAY_PROTOCOL_ERROR
            })
        );
        assert_eq!(client.frame().await?, None);
        assert!(connection.accept().await.is_none());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn idle_connections_get_closed() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, Some(Duration::from_secs(60)))?;
        time::sleep(Duration::from_secs(50)).await;
        // Any frame keeps the connection open, also a ping.
        client.send(Frame::Ping { id: 1 }).await?;
        assert_eq!(client.frame().await?, Some(Frame::Ping { id: 1 }));
        time::sleep(Duration::from_secs(50)).await;
        assert!(
            time::timeout(Duration::from_secs(1), connection.accept())
                .await
                .is_err()
        );

        assert_eq!(
            client.frame().await?,
            Some(Frame::GoAway {
                last_stream_id: 0,
                status: GOAWAY_OK
            })
        );
        assert!(connection.accept().await.is_none());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeouts_end_connections_with_stuck_streams() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, Some(Duration::from_secs(30)))?;
        client.open(1, [("streamtype", "data")].into()).await?;
        // Accepted, but never read.
        let _stream = connection.accept().await.context("stream")?;
        client.frame().await?;
        for _ in 0..STREAM_QUEUE + 2 {
            client
                .send(Frame::Data {
                    stream_id: 1,
                    fin: false,
                    data: Bytes::from_static(b"data"),
                })
                .await?;
        }

        assert_eq!(
            client.frame().await?,
            Some(Frame::GoAway {
                last_stream_id: 1,
                status: GOAWAY_OK
            })
        );
        assert!(connection.accept().await.is_none());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn huge_idle_timeouts_are_none() -> Result<()> {
        let (mut client, _connection) = connect(accept_data, Some(Duration::MAX))?;
        time::sleep(Duration::from_secs(3600)).await;
        client.send(Frame::Ping { id: 1 }).await?;
        assert_eq!(client.frame().await?, Some(Frame::Ping { id: 1 }));
        Ok(())
    }

    #[tokio::test]
    async fn resets_get_through_a_busy_connection() -> Result<()> {
        // A client which does not read: the writer waits for the connection
        // and the pings fill the control queue.
        let (client, server) = duplex(64);
        let mut connection = Connection::serve(server, None, accept_data);
        let mut client = Client::new(client)?;
        client.open(1, [("streamtype", "data")].into()).await?;
        let stream = connection.accept().await.context("stream")?;
        let pings = async {
            for id in (1..2 * CONTROL_QUEUE as u32).step_by(2) {
                client.send(Frame::Ping { id }).await?;
            }
            anyhow::Ok(())
        };
        let _ = time::timeout(Duration::from_millis(100), pings).await;
        drop(stream);

        // Once the client reads again, the reset arrives.
        let reset = async {
            loop {
                match client.frame().await? {
                    Some(Frame::RstStream { stream_id: 1, .. }) => return anyhow::Ok(()),
                    Some(_) => {}
                    None => bail!("connection ended without the reset"),
                }
            }
        };
        time::timeout(Duration::from_secs(5), reset).await??;
        Ok(())
    }

    #[tokio::test]
    async fn slow_streams_do_not_lose_data() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, None)?;
        client.open(1, [("streamtype", "data")].into()).await?;
        let stream = connection.accept().await.context("stream")?;
        client.frame().await?;

        // More frames than the stream queue holds, which the reader waits
        // with.
        let frames = STREAM_QUEUE * 4;
        for i in 0..frames {
            client
                .send(Frame::Data {
                    stream_id: 1,
                    fin: i + 1 == frames,
                    data: Bytes::from(vec![i as u8; 1024]),
                })
                .await?;
        }
        let (mut receiver, _sender) = stream.split();
        let mut received = 0;
        while let Some(data) = receiver.recv().await {
            assert_eq!(data, vec![received as u8; 1024]);
            received += 1;
        }
        assert_eq!(received, frames);
        Ok(())
    }

    #[tokio::test]
    async fn streams_opened_with_fin_have_no_data() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, None)?;
        client
            .send(Frame::SynStream {
                stream_id: 1,
                fin: true,
                headers: [("streamtype", "data")].into(),
            })
            .await?;
        let (mut receiver, _sender) = connection.accept().await.context("stream")?.split();
        assert_eq!(receiver.recv().await, None);
        assert!(!receiver.is_reset());
        Ok(())
    }
}
