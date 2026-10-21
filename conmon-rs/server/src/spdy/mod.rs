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
        atomic::{AtomicU32, AtomicUsize, Ordering},
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

/// How often the server pings the client, like the kubelet does. A client
/// which went away shows up as a failed write, also while the reader waits
/// for a stream which does not take its data and misses the end of the
/// connection.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// A connection a client upgraded to SPDY/3.1.
pub struct Connection {
    streams: mpsc::Receiver<Stream>,
}

impl Connection {
    /// Serve SPDY on the connection. The client opens streams, `accept`
    /// decides about each by its headers, rejected ones get reset. The
    /// connection ends when the client closes it, violates the protocol, a
    /// ping fails, or when no frame went either way for the idle timeout.
    /// The pings of the server do not count, see [`PING_INTERVAL`].
    pub fn serve<T, F>(
        io: T,
        idle_timeout: Option<Duration>,
        ping_interval: Option<Duration>,
        accept: F,
    ) -> Self
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
        if let Some(interval) = ping_interval {
            tokio::spawn(Self::ping(shared.clone(), interval));
        }
        tokio::spawn(Self::write(shared, writer, encoder, control_rx, data_rx));

        Self {
            streams: streams_rx,
        }
    }

    /// The next stream the client opened, `None` once the connection ended.
    pub async fn accept(&mut self) -> Option<Stream> {
        self.streams.recv().await
    }

    /// Ping the client until the connection ends. The pings of the server
    /// have even IDs, the client echoes them.
    async fn ping(shared: Arc<Shared>, interval: Duration) {
        let mut interval = time::interval(interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        interval.tick().await;
        let mut id: u32 = 0;
        loop {
            select! {
                _ = interval.tick() => {}
                _ = shared.done.cancelled() => return,
            }
            id = id.wrapping_add(2) & 0x7fff_ffff;
            // A full queue goes out anyway, which tells as much.
            let _ = shared.control.try_send(Frame::Ping { id });
        }
    }

    /// Whether a frame keeps the connection from being idle, which the
    /// pings of the server do not.
    fn is_activity(frame: &Frame) -> bool {
        !matches!(frame, Frame::Ping { id } if id % 2 == 0)
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

    /// Write the queued frames: control frames first, then the resets of
    /// streams without queued data, then data. The data frames of a stream
    /// keep their order, its FIN goes through the same queue, and a reset of a
    /// stream with queued data follows its last frame. A reset follows the
    /// SYN_REPLY of its stream, which is queued before the stream exists.
    async fn write<T>(
        shared: Arc<Shared>,
        mut writer: WriteHalf<T>,
        mut encoder: Encoder,
        mut control_rx: mpsc::Receiver<Frame>,
        mut data_rx: mpsc::Receiver<QueuedData>,
    ) where
        T: AsyncWrite,
    {
        let mut buf = BytesMut::with_capacity(WRITE_BUFFER);
        let result = async {
            loop {
                let mut active = false;
                let mut control = |frame: &Frame, buf: &mut BytesMut| {
                    active |= Self::is_activity(frame);
                    encoder.encode(frame, buf)
                };
                let data = select! {
                    biased;
                    _ = shared.done.cancelled() => break,
                    Some(frame) = control_rx.recv() => {
                        control(&frame, &mut buf)?;
                        None
                    }
                    _ = shared.resets_ready.notified() => None,
                    Some(data) = data_rx.recv() => Some(data),
                    else => break,
                };
                // Whatever else is queued already goes out with it.
                while let Ok(frame) = control_rx.try_recv() {
                    control(&frame, &mut buf)?;
                }
                for stream_id in shared.take_resets() {
                    control(&Self::reset_frame(stream_id), &mut buf)?;
                }
                if let Some(data) = data {
                    active = true;
                    Self::encode_data(&mut encoder, &data, &mut buf)?;
                }
                while buf.len() < WRITE_BUFFER {
                    let Ok(data) = data_rx.try_recv() else {
                        break;
                    };
                    active = true;
                    Self::encode_data(&mut encoder, &data, &mut buf)?;
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
                if active {
                    shared.activity.touch();
                }
            }

            // The reader queues a GOAWAY or resets before it ends the
            // connection, the streams may have queued their last data. Data
            // of existing streams may follow the GOAWAY.
            while let Ok(frame) = control_rx.try_recv() {
                encoder.encode(&frame, &mut buf)?;
            }
            for stream_id in shared.take_resets() {
                encoder.encode(&Self::reset_frame(stream_id), &mut buf)?;
            }
            while let Ok(data) = data_rx.try_recv() {
                Self::encode_data(&mut encoder, &data, &mut buf)?;
            }
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

    /// Encode a data frame, followed by the reset of its stream if it was the
    /// last data the stream queued before its reset.
    fn encode_data(encoder: &mut Encoder, data: &QueuedData, buf: &mut BytesMut) -> Result<()> {
        encoder.encode(&data.frame, buf)?;
        if data.queued.remove()
            && let Frame::Data { stream_id, .. } = data.frame
        {
            encoder.encode(&Self::reset_frame(stream_id), buf)?;
        }
        Ok(())
    }

    fn reset_frame(stream_id: u32) -> Frame {
        Frame::RstStream {
            stream_id,
            status: STATUS_CANCEL,
        }
    }
}

/// A data frame in the data queue, with the count of its stream.
struct QueuedData {
    frame: Frame,
    queued: Arc<Queued>,
}

/// The data frames a stream has in the data queue, and whether it got reset
/// after them. Both share one word, so that the writer and the reset agree
/// on who sends the RST_STREAM.
#[derive(Default)]
struct Queued(AtomicUsize);

impl Queued {
    const RESET: usize = 1 << (usize::BITS - 1);

    /// A frame goes into the data queue.
    fn add(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }

    /// A frame left the data queue or did not make it, whether the reset of
    /// the stream is due now.
    fn remove(&self) -> bool {
        self.0.fetch_sub(1, Ordering::AcqRel) == Self::RESET | 1
    }

    /// Reset the stream after its queued frames, whether the reset is due
    /// right away because there are none.
    fn reset(&self) -> bool {
        self.0.fetch_or(Self::RESET, Ordering::AcqRel) & !Self::RESET == 0
    }

    #[cfg(test)]
    fn frames(&self) -> usize {
        self.0.load(Ordering::Acquire) & !Self::RESET
    }
}

/// Takes back the count of a frame which did not make it into the data
/// queue, also when its send got cancelled.
struct Unqueued<'a> {
    sender: &'a StreamSender,
    queued: bool,
}

impl Drop for Unqueued<'_> {
    fn drop(&mut self) {
        if !self.queued && self.sender.queued.remove() {
            self.sender.shared.queue_reset(self.sender.id);
        }
    }
}

/// What the reader, the writer and the streams of a connection share.
struct Shared {
    control: mpsc::Sender<Frame>,
    data: mpsc::Sender<QueuedData>,
    streams: Mutex<HashMap<u32, StreamState>>,

    /// The streams the server reset without queued data, which the writer
    /// sends next. Unlike the control queue it cannot be full, a stream gets
    /// reset only once.
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

    fn take_resets(&self) -> Vec<u32> {
        mem::take(&mut *self.resets.lock().unwrap_or_else(|e| e.into_inner()))
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
            if Connection::is_activity(&frame) {
                self.shared.activity.touch();
            }
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
                // Pings of the client have odd IDs and get echoed, even ones are
                // the echoes of the pings of the server.
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
                queued: Arc::default(),
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

    /// The frames of the stream in the data queue.
    queued: Arc<Queued>,
    closed: bool,
}

impl StreamSender {
    /// Send data in a single frame, at most [`frame::MAX_DATA_FRAME_LEN`]
    /// bytes. Fails once the stream got reset or the connection ended.
    pub async fn send(&mut self, data: Bytes) -> Result<()> {
        if self.closed {
            bail!("SPDY stream {} is closed", self.id);
        }
        self.queue(false, data).await
    }

    /// Queue a data frame for the writer.
    async fn queue(&self, fin: bool, data: Bytes) -> Result<()> {
        self.queued.add();
        let mut unqueued = Unqueued {
            sender: self,
            queued: false,
        };
        let frame = QueuedData {
            frame: Frame::Data {
                stream_id: self.id,
                fin,
                data,
            },
            queued: self.queued.clone(),
        };
        select! {
            biased;
            _ = self.reset.cancelled() => bail!("SPDY stream {} got reset", self.id),
            result = self.shared.data.send(frame) => {
                result.map_err(|_| anyhow!("SPDY connection closed"))?;
                unqueued.queued = true;
                Ok(())
            }
        }
    }

    /// Close the sending side after the data sent before. A close which got
    /// cancelled leaves the stream to its drop, which resets it.
    pub async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let result = self.queue(true, Bytes::new()).await;
        self.closed = true;
        self.shared.local_closed(self.id);
        result
    }

    /// Reset the stream, both sides end. The data queued for the stream
    /// before goes out first.
    pub fn reset(&mut self) {
        if self.closed && self.reset.is_cancelled() {
            return;
        }
        self.closed = true;
        if !self.reset.is_cancelled() && self.queued.reset() {
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
        Ok((
            client,
            Connection::serve(server, idle_timeout, None, accept),
        ))
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
        let mut connection = Connection::serve(server, None, None, accept_data);
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
    async fn queued_data_goes_out_when_the_connection_ends() -> Result<()> {
        let (mut client, mut connection) = connect(accept_data, None)?;
        client.open(1, [("streamtype", "data")].into()).await?;
        let stream = connection.accept().await.context("stream")?;
        client.frame().await?;

        // Queued without yielding to the writer, which then sees the end of
        // the connection first.
        let (_receiver, mut sender) = stream.split();
        sender.send(Bytes::from_static(b"last")).await?;
        sender.shared.done.cancel();

        assert_eq!(
            client.frame().await?,
            Some(Frame::Data {
                stream_id: 1,
                fin: false,
                data: Bytes::from_static(b"last")
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn resets_follow_the_queued_data() -> Result<()> {
        for shutdown in [false, true] {
            let (mut client, mut connection) = connect(accept_data, None)?;
            client.open(1, [("streamtype", "data")].into()).await?;
            client.open(3, [("streamtype", "data")].into()).await?;
            let first = connection.accept().await.context("stream")?;
            let mut second = connection.accept().await.context("stream")?;
            client.frame().await?;
            client.frame().await?;

            // Queued without yielding to the writer.
            let (_receiver, mut sender) = first.split();
            sender.send(Bytes::from_static(b"first")).await?;
            second.send(Bytes::from_static(b"second")).await?;
            let shared = sender.shared.clone();
            sender.reset();
            if shutdown {
                shared.go_away(GOAWAY_OK);
                // Data of existing streams may follow the GOAWAY.
                assert!(matches!(client.frame().await?, Some(Frame::GoAway { .. })));
            }

            assert_eq!(
                client.frame().await?,
                Some(Frame::Data {
                    stream_id: 1,
                    fin: false,
                    data: Bytes::from_static(b"first")
                })
            );
            assert!(matches!(
                client.frame().await?,
                Some(Frame::RstStream { stream_id: 1, .. })
            ));
            assert_eq!(
                client.frame().await?,
                Some(Frame::Data {
                    stream_id: 3,
                    fin: false,
                    data: Bytes::from_static(b"second")
                })
            );
            // Nothing is left behind for the streams.
            assert_eq!(sender.queued.frames(), 0);
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_sends_do_not_lose_the_reset() -> Result<()> {
        for close in [false, true] {
            // A client which does not read, so that the data queue fills up.
            let (client, server) = duplex(64);
            let mut connection = Connection::serve(server, None, None, accept_data);
            let mut client = Client::new(client)?;
            client.open(1, [("streamtype", "data")].into()).await?;
            let (_receiver, mut sender) = connection.accept().await.context("stream")?.split();
            let shared = sender.shared.clone();

            // Until a send has to wait, which gets cancelled.
            while time::timeout(
                Duration::from_millis(50),
                sender.send(Bytes::from(vec![0; 1024])),
            )
            .await
            .is_ok()
            {}
            if close {
                assert!(
                    time::timeout(Duration::from_millis(50), sender.close())
                        .await
                        .is_err()
                );
                // Not closed, so the drop resets it.
                drop(sender);
            } else {
                sender.reset();
            }

            // The reset follows the queued data once the client reads.
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
            assert!(shared.lock_streams().is_empty());
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn the_server_pings_its_clients() -> Result<()> {
        let (mut client, server) = Client::pair()?;
        let _connection = Connection::serve(
            server,
            Some(Duration::from_secs(1)),
            Some(Duration::from_millis(100)),
            accept_data,
        );
        let Some(Frame::Ping { id }) = client.frame().await? else {
            bail!("no ping");
        };
        assert_eq!(id % 2, 0);
        // Its own pings do not keep the connection from being idle.
        client.send(Frame::Ping { id }).await?;
        loop {
            match client.frame().await? {
                Some(Frame::Ping { id }) if id % 2 == 0 => {}
                frame => {
                    assert!(matches!(frame, Some(Frame::GoAway { .. })), "{frame:?}");
                    break;
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn gone_clients_end_connections_with_stuck_streams() -> Result<()> {
        let (mut client, server) = Client::pair()?;
        let mut connection =
            Connection::serve(server, None, Some(Duration::from_millis(100)), accept_data);
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

        // Nothing reads the end of the connection, the next ping fails.
        drop(client);
        let end = time::timeout(Duration::from_secs(5), connection.accept()).await?;
        assert!(end.is_none());
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
