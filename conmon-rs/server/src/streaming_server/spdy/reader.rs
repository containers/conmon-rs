//! Reads the frames of the client and hands them to the streams.

use super::{
    Connection, Live, SHUTDOWN_TIMEOUT, STREAM_QUEUE, Shared, Stream, StreamReceiver, StreamSender,
    StreamState,
    frame::{
        Decoder, Frame, GOAWAY_PROTOCOL_ERROR, Headers, STATUS_CANCEL, STATUS_PROTOCOL_ERROR,
        STATUS_REFUSED_STREAM,
    },
};
use anyhow::{Result, bail};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, ReadHalf},
    select,
    sync::mpsc,
    time,
};
use tracing::{debug, trace, warn};

/// Reads the frames of the client and dispatches them to the streams.
pub(super) struct Reader<T, F> {
    pub(super) shared: Arc<Shared>,
    pub(super) reader: ReadHalf<T>,
    pub(super) accept: F,
    pub(super) streams: mpsc::Sender<Stream>,
}

/// Why the reader stopped.
enum End {
    /// The client closed the connection.
    ClientClosed,

    /// Nothing takes streams anymore or the connection ended otherwise, like
    /// when it was idle.
    Closed,

    /// The client violated the protocol, the connection cannot continue.
    ProtocolError(anyhow::Error),
}

impl<T, F> Reader<T, F>
where
    T: AsyncRead,
    F: Fn(&Headers) -> Result<()>,
{
    pub(super) async fn run(mut self) {
        let shared = self.shared.clone();
        let end = self.read().await;
        if let End::ProtocolError(e) = &end {
            warn!("Closing SPDY connection: {e:#}");
            shared.go_away(GOAWAY_PROTOCOL_ERROR);
        }

        shared.done.cancel();
        // Ends the data of all streams, the reset tokens are children of
        // `done` and got cancelled with it.
        shared.lock_streams().clear();

        // Nothing gets accepted anymore.
        drop(self.streams);
        if !matches!(end, End::ClientClosed) {
            Self::drain(&mut self.reader).await;
        }
    }

    /// Read the client after the end of the connection until it closed it
    /// as well, for up to [`SHUTDOWN_TIMEOUT`]. A socket closed with input
    /// it did not read resets the connection, which can drop the last
    /// frames of the server, like the status of a session.
    async fn drain(reader: &mut ReadHalf<T>) {
        let mut buf = [0; 4096];
        let drained = async { while matches!(reader.read(&mut buf).await, Ok(1..)) {} };
        if time::timeout(SHUTDOWN_TIMEOUT, drained).await.is_err() {
            debug!("SPDY client did not close the connection in time");
        }
    }

    async fn read(&mut self) -> End {
        let mut decoder = Decoder::default();
        let mut going_away = false;

        loop {
            let frame = select! {
                frame = decoder.read(&mut self.reader) => frame,
                () = self.shared.done.cancelled() => return End::Closed,
            };
            let frame = match frame {
                Ok(Some(frame)) => frame,
                Ok(None) => return End::ClientClosed,
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
    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "a shared reference would need the reader to be Sync for a Send future"
    )]
    async fn open(
        &mut self,
        stream_id: u32,
        fin: bool,
        headers: Headers,
        going_away: bool,
    ) -> Result<()> {
        let refused = if going_away || self.streams.is_closed() {
            Some(STATUS_REFUSED_STREAM)
        } else if !self.shared.has_room() {
            debug!("Refusing SPDY stream {stream_id}, too many streams are open");
            Some(STATUS_REFUSED_STREAM)
        } else if let Err(e) = (self.accept)(&headers) {
            debug!("Rejecting SPDY stream {stream_id}: {e:#}");
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
        let remote_closed = Arc::new(AtomicBool::new(fin));
        let (data_tx, data_rx) = mpsc::channel(STREAM_QUEUE);
        self.shared.lock_streams().insert(
            stream_id,
            StreamState {
                data: (!fin).then_some(data_tx),
                reset: reset.clone(),
                remote_closed: remote_closed.clone(),
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

        let live = Live::new(&self.shared);
        let stream = Stream {
            headers,
            receiver: StreamReceiver {
                data: data_rx,
                reset: reset.clone(),
                remote_closed,
                _live: live.clone(),
            },
            sender: StreamSender {
                id: stream_id,
                shared: self.shared.clone(),
                reset,
                queued: Arc::default(),
                closed: false,
                _live: live,
            },
        };
        select! {
            // Dropping a stream nobody accepts anymore resets it.
            _ = self.streams.send(stream) => Ok(()),
            () = self.shared.done.cancelled() => bail!("connection closed"),
        }
    }
}
