//! A SPDY/3.1 server for the streaming protocols of Kubernetes. The kubelet
//! still talks SPDY to the streaming server of a container runtime, also when
//! the client used a WebSocket, which the API server or the kubelet unwrap.
//!
//! Only what moby/spdystream, the SPDY implementation of Kubernetes, uses is
//! supported: the client opens all streams, the server answers pings. There
//! is no flow control, spdystream ignores window updates, so the server
//! bounds what it buffers itself and stops reading the connection when a
//! stream does not take its data, or when the streams together hold
//! [`DATA_BUFFER`] bytes. Streams have to take their data while they wait for
//! something else, otherwise they hold up all others. A single stream can
//! hold only a part of the budget, so that a slow stream alone does not stop
//! the connection earlier than without it, but a few slow streams together
//! do.

mod frame;
mod reader;
mod stream;

use crate::{
    mutex::lock,
    streaming_server::{
        budget::{Budget, Held},
        idle::Activity,
    },
};
use anyhow::{Context, Result, anyhow, bail};
use bytes::{Bytes, BytesMut};
pub use frame::Headers;
use frame::{Encoder, Frame, GOAWAY_OK, STATUS_CANCEL};
use reader::Reader;
use std::{
    collections::HashMap,
    mem,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
};
pub use stream::{Stream, StreamReceiver, StreamSender};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, WriteHalf},
    select,
    sync::{Notify, mpsc},
    time::{self, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

/// How many control frames wait for the connection.
const CONTROL_QUEUE: usize = 64;

/// How many data frames of all streams wait for the connection.
const DATA_QUEUE: usize = 16;

/// How many data frames of a stream wait for the stream to take them, before
/// the connection is not read anymore. The decoder hands on the data frames
/// of the client in parts of at most 64 KiB, also larger ones, so a stream
/// holds at most 1 MiB.
const STREAM_QUEUE: usize = 16;

/// How many bytes of the client all streams of a connection hold together,
/// before the connection is not read anymore. Without it, a client could
/// make the server hold [`STREAM_QUEUE`] frames for each of [`MAX_STREAMS`]
/// streams which do not take their data, like the data streams of port
/// forward requests waiting for their error streams. The part which the
/// reader waits to place comes on top.
const DATA_BUFFER: u32 = 4 * 1024 * 1024;

/// How many accepted streams wait for [`Connection::accept`].
const ACCEPT_QUEUE: usize = 16;

/// The most streams a client may have open at once. A Kubernetes port forward
/// uses two per forwarded connection. A stream counts until the server let
/// go of it, also after the client reset it, so that a client which opens
/// and resets streams cannot make the server hold more of them.
pub(crate) const MAX_STREAMS: usize = 1024;

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
    shared: Arc<Shared>,
}

impl Connection {
    /// Serve SPDY on the connection. The client opens streams, `accept`
    /// decides about each by its headers, rejected ones get reset. The
    /// connection ends when the client closes it, violates the protocol, a
    /// write fails, like of a ping, or when it was idle for the timeout.
    /// Nothing waits for the echoes of the pings. Only the frames of the
    /// client keep the connection active, except for these echoes, and the
    /// data of the server, not its control frames, see [`Self::is_activity`].
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
            budget: Budget::new(DATA_BUFFER),
            resets: Mutex::default(),
            resets_ready: Notify::new(),
            closing: CancellationToken::new(),
            done: CancellationToken::new(),
            activity: Activity::new(),
            last_stream_id: AtomicU32::new(0),
            live_streams: AtomicUsize::new(0),
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
        // its data. Ends the connection once it was idle for the timeout.
        shared.activity.watch(idle_timeout, shared.done.clone(), {
            let shared = shared.clone();
            move || {
                debug!("Closing idle SPDY connection");
                shared.go_away(GOAWAY_OK);
            }
        });
        if let Some(interval) = ping_interval {
            tokio::spawn(Self::ping(shared.clone(), interval));
        }
        tokio::spawn(Self::write(
            shared.clone(),
            writer,
            encoder,
            control_rx,
            data_rx,
        ));

        Self {
            streams: streams_rx,
            shared,
        }
    }

    /// The next stream the client opened, `None` once the connection ended.
    pub async fn accept(&mut self) -> Option<Stream> {
        self.streams.recv().await
    }

    /// End the connection right away.
    pub fn abort(&self) {
        self.handle().abort();
    }

    /// Cancelled once the connection ended.
    pub fn done(&self) -> CancellationToken {
        self.handle().done()
    }

    /// When the connection was active the last time, for an idle timeout
    /// which its user applies instead of [`Connection::serve`], see there.
    pub fn activity(&self) -> Activity {
        self.shared.activity.clone()
    }

    /// A handle to close the connection, while the connection itself still
    /// accepts streams.
    pub fn handle(&self) -> ConnectionHandle {
        ConnectionHandle(self.shared.clone())
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
                () = shared.done.cancelled() => return,
            }
            id = id.wrapping_add(2) & 0x7fff_ffff;
            // A full queue goes out anyway, which tells as much.
            let _ = shared.control.try_send(Frame::Ping { id });
        }
    }

    /// Whether a frame of the client keeps the connection from being idle,
    /// which its answers to the pings of the server do not. Of the frames of
    /// the server only data counts, see [`Self::write`].
    fn is_activity(frame: &Frame) -> bool {
        !matches!(frame, Frame::Ping { id } if id % 2 == 0)
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
        let mut closing = false;
        let result = async {
            loop {
                // Only data keeps the connection active, not the control
                // frames of the server, like resets of the streams of a
                // client which keeps failing.
                let mut active = false;
                let data = select! {
                    biased;
                    () = shared.done.cancelled() => break,
                    Some(frame) = control_rx.recv() => {
                        encoder.encode(&frame, &mut buf)?;
                        None
                    }
                    () = shared.resets_ready.notified() => None,
                    Some(data) = data_rx.recv() => Some(data),
                    // The server closes the connection, the frames queued
                    // until now go out first.
                    () = shared.closing.cancelled(), if !closing => {
                        closing = true;
                        None
                    }
                    else => break,
                };
                // Whatever else is queued already goes out with it.
                while let Ok(frame) = control_rx.try_recv() {
                    encoder.encode(&frame, &mut buf)?;
                }
                for stream_id in shared.take_resets() {
                    encoder.encode(&Self::reset_frame(stream_id), &mut buf)?;
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
                if !buf.is_empty() {
                    select! {
                        biased;
                        result = writer.write_all(&buf) => result.context("write frames")?,
                        // A client which does not read anymore must not keep
                        // the connection open.
                        () = shared.done.cancelled() => return Ok(()),
                    }
                    buf.clear();
                    if active {
                        shared.activity.touch();
                    }
                }
                // The rest goes out with the shutdown below.
                if closing {
                    break;
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

/// Closes or ends a connection, see [`Connection::handle`]. Once the
/// [`Connection`] is gone, the streams the client opens get refused, the ones
/// nobody took yet get reset.
#[derive(Clone)]
pub struct ConnectionHandle(Arc<Shared>);

impl ConnectionHandle {
    /// Close the connection once the frames queued so far went out.
    pub fn close(&self) {
        self.0.closing.cancel();
    }

    /// End the connection right away.
    pub fn abort(&self) {
        self.0.done.cancel();
    }

    /// Cancelled once the connection ended.
    pub fn done(&self) -> CancellationToken {
        self.0.done.clone()
    }
}

/// What the reader, the writer and the streams of a connection share.
struct Shared {
    control: mpsc::Sender<Frame>,
    data: mpsc::Sender<QueuedData>,
    streams: Mutex<HashMap<u32, StreamState>>,

    /// The bytes of the client all streams hold, see [`DATA_BUFFER`].
    budget: Budget,

    /// The streams the server reset without queued data, which the writer
    /// sends next. Unlike the control queue it cannot be full, a stream gets
    /// reset only once.
    resets: Mutex<Vec<u32>>,
    resets_ready: Notify,

    /// Cancelled once the server closes the connection after the frames
    /// queued until then.
    closing: CancellationToken,

    /// Cancelled once the connection ended.
    done: CancellationToken,

    /// When the last frame got read or written, for the idle timeout.
    activity: Activity,

    /// The last stream the client opened, for the GOAWAY.
    last_stream_id: AtomicU32,

    /// The streams the server holds, see [`Live`].
    live_streams: AtomicUsize,
}

/// An open stream as the reader sees it.
struct StreamState {
    /// The data of the client, `None` once the client closed its side.
    data: Option<mpsc::Sender<Held>>,

    /// Cancelled when the stream got reset or the connection ended.
    reset: CancellationToken,

    /// Set once the client closed its side, shared with the receiver.
    remote_closed: Arc<AtomicBool>,

    /// Whether the server closed its side.
    local_closed: bool,
}

impl Shared {
    fn lock_streams(&self) -> MutexGuard<'_, HashMap<u32, StreamState>> {
        lock(&self.streams)
    }

    /// Queue the reset of a stream for the writer.
    fn queue_reset(&self, id: u32) {
        lock(&self.resets).push(id);
        self.resets_ready.notify_one();
    }

    fn take_resets(&self) -> Vec<u32> {
        mem::take(&mut *lock(&self.resets))
    }

    /// The client closed its side of the stream.
    fn remote_closed(&self, id: u32) {
        let mut streams = self.lock_streams();
        if let Some(state) = streams.get_mut(&id) {
            // Before the data ends, which the receiver may see right away.
            state.remote_closed.store(true, Ordering::Release);
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
    /// does not take it or the streams hold all of the budget, which stops
    /// reading the connection.
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
            // A stream which went away drops its data, which gives its share
            // of the budget back.
            result = async { tx.send(self.budget.hold(data).await).await } => {
                // A stream which takes its data slowly keeps the connection
                // active, only one which does not take it at all lets it
                // go idle.
                if result.is_ok() {
                    self.activity.touch();
                }
            }
            // A stream which went away while waiting for the budget.
            () = tx.closed() => {}
            () = reset.cancelled() => {}
        }
    }

    async fn send_control(&self, frame: Frame) -> Result<()> {
        select! {
            result = self.control.send(frame) => result.map_err(|_| anyhow!("connection closed")),
            () = self.done.cancelled() => bail!("connection closed"),
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
        // Not under the lock, which the reader and all streams take.
        let state = self.lock_streams().remove(&id);
        if let Some(state) = state {
            state.reset.cancel();
        }
    }

    /// Whether the client may open another stream, see [`MAX_STREAMS`].
    fn has_room(&self) -> bool {
        self.lock_streams().len() < MAX_STREAMS
            && self.live_streams.load(Ordering::Acquire) < MAX_STREAMS
    }
}

/// Counts a stream in [`Shared::live_streams`] until both of its halves got
/// dropped, the stream may hold its headers and data until then.
struct Live(Arc<Shared>);

impl Live {
    fn new(shared: &Arc<Shared>) -> Arc<Self> {
        shared.live_streams.fetch_add(1, Ordering::AcqRel);
        Arc::new(Self(shared.clone()))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.live_streams.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The entry points of the fuzz targets.
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzzing;

/// A SPDY client for the tests of the protocols on top.
#[cfg(test)]
pub mod testing;

#[cfg(test)]
mod tests;
