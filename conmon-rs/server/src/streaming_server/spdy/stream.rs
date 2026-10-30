//! The streams of a connection, split into the halves the protocols on
//! top use.

use super::{
    Live, Queued, QueuedData, Shared,
    frame::{Frame, Headers},
};
use crate::streaming_server::budget::Held;
use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::{select, sync::mpsc};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// A stream the client opened and the server accepted.
pub struct Stream {
    pub(super) headers: Headers,
    pub(super) receiver: StreamReceiver,
    pub(super) sender: StreamSender,
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

    /// Whether the stream got reset or the connection ended.
    pub fn is_reset(&self) -> bool {
        self.receiver.is_reset()
    }

    /// Resolves once the stream got reset or the connection ended, also
    /// after the stream got dropped.
    pub fn reset_watch(&self) -> WaitForCancellationFutureOwned {
        self.receiver.reset.clone().cancelled_owned()
    }

    /// Split the stream into the data of the client and the sending side,
    /// to use both at the same time.
    pub fn split(self) -> (StreamReceiver, StreamSender) {
        (self.receiver, self.sender)
    }

    /// Send data, see [`StreamSender::send`].
    #[cfg(test)]
    pub async fn send(&mut self, data: Bytes) -> Result<()> {
        self.sender.send(data).await
    }

    /// Close the sending side, see [`StreamSender::close`].
    #[cfg(test)]
    pub async fn close(&mut self) -> Result<()> {
        self.sender.close().await
    }
}

/// The data the client sends on a stream.
pub struct StreamReceiver {
    pub(super) data: mpsc::Receiver<Held>,
    pub(super) reset: CancellationToken,
    pub(super) remote_closed: Arc<AtomicBool>,
    pub(super) _live: Arc<Live>,
}

impl StreamReceiver {
    /// The next data, `None` once the client closed its side, the stream got
    /// reset or the connection ended.
    pub async fn recv(&mut self) -> Option<Bytes> {
        self.data.recv().await.map(Held::into_bytes)
    }

    /// The next data like [`Self::recv`], which still counts against the
    /// budget of the connection while it is held.
    pub async fn recv_held(&mut self) -> Option<Held> {
        self.data.recv().await
    }

    /// Whether the stream got reset or the connection ended.
    pub fn is_reset(&self) -> bool {
        self.reset.is_cancelled()
    }

    /// Whether the client closed its side, also if the stream got reset or
    /// the connection ended afterwards.
    pub fn is_closed_by_client(&self) -> bool {
        self.remote_closed.load(Ordering::Acquire)
    }
}

/// The sending side of a stream. Dropping it without closing it resets the
/// stream.
pub struct StreamSender {
    pub(super) id: u32,
    pub(super) shared: Arc<Shared>,
    pub(super) reset: CancellationToken,

    /// The frames of the stream in the data queue.
    pub(super) queued: Arc<Queued>,
    pub(super) closed: bool,
    pub(super) _live: Arc<Live>,
}

impl StreamSender {
    /// Send data in a single frame, at most
    /// [`MAX_DATA_FRAME_LEN`](super::frame::MAX_DATA_FRAME_LEN) bytes. Fails
    /// once the stream got reset or the connection ended.
    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "one send at a time keeps the frames in order"
    )]
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
            () = self.reset.cancelled() => bail!("SPDY stream {} got reset", self.id),
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
    /// before goes out first. Users reset a stream by dropping it without
    /// closing it, the tests also directly.
    pub(super) fn reset(&mut self) {
        self.closed = true;
        if !self.reset.is_cancelled() && self.queued.reset() {
            self.shared.queue_reset(self.id);
        }
        self.shared.remove(self.id);
    }

    /// Resolves once the client reset the stream or the connection ended.
    pub async fn reset_by_peer(&self) {
        self.reset.cancelled().await;
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
