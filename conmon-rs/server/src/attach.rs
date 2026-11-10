//! The attach sockets of the containers, see the attach socket section of
//! docs/protocols.md: the stdin of the clients towards the container and the
//! container output towards the clients.

mod socket;

use self::socket::create_listener;
use crate::{
    container_io::Message,
    fs_util::{FileId, RemoveError},
    mutex::lock,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    pin::pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence},
    },
};
use tokio::{
    select,
    sync::{
        Notify,
        broadcast::{self, Receiver, Sender},
    },
    task,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

/// Capacity of the stdin channel towards the container.
const STDIN_CHANNEL_SIZE: usize = 16;

/// Capacity of the output broadcast channel towards the attach clients. A
/// client which falls further behind than this loses the oldest output,
/// which gets logged with the number of skipped messages, see
/// [`Lag`](crate::container_io::Lag). The container output never waits for
/// an attach client, a slow or stuck client must not block the container or
/// the other clients, and the server does not buffer output without bound
/// for it either. About what the stdio channels buffer for exec, as a power
/// of two, which tokio rounds the capacity of a broadcast channel up to
/// anyway.
pub(crate) const OUTPUT_CHANNEL_SIZE: usize = 128;

/// A shared container attach abstraction.
#[derive(Clone, Debug)]
pub(crate) struct SharedContainerAttach {
    // A bounded channel instead of a broadcast, so that a slow container stdin
    // applies backpressure to the attach clients instead of dropping input.
    read_half_rx: crate::channel::Receiver<Bytes>,
    read_half_tx: crate::channel::Sender<Bytes>,

    /// The output towards the attach clients.
    output: Arc<OutputState>,

    /// Cancelled once a client with `stop_after_stdin_eof` closed its stdin,
    /// which closes the stdin of the container for good. It is separate from
    /// the token of the container, which only the exit may cancel.
    stdin_closed: CancellationToken,
    stop_after_stdin_eof: Arc<AtomicBool>,
    terminal: bool,

    /// Cancelled once nothing takes the output anymore, see
    /// [`SharedContainerAttach::stop_output`].
    output_stopped: CancellationToken,

    /// The listening attach sockets by path.
    listeners: Arc<Mutex<HashMap<PathBuf, AttachListener>>>,
}

/// A listening attach socket and the identity of its file, which tells if
/// the file got removed or replaced meanwhile. The inode stays in use while
/// the listener is open, so its number cannot be reused until then.
#[derive(Debug)]
struct AttachListener {
    /// Tells the listeners of a path apart.
    id: u64,
    file: FileId,

    /// Stops accepting connections, the accepted ones stay.
    stop: CancellationToken,
}

impl AttachListener {
    fn next_id() -> u64 {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    }

    /// Whether the socket file at the path is still the one listened on.
    fn is_bound_at(&self, path: &Path) -> bool {
        self.file.is_at(path)
    }
}

/// The output towards the attach clients, shared with the registered output
/// of the container, see [`SharedContainerAttach::output`].
#[derive(Debug, Default)]
struct OutputState {
    /// The sender of the broadcast, created for the first attach client, a
    /// container without any does not need its buffer. It is never removed,
    /// so the output reads it without a lock.
    tx: OnceLock<Sender<Message>>,

    /// Set once the output ended, before the done message gets sent.
    ended: AtomicBool,

    /// Set once an output got registered, see
    /// [`SharedContainerAttach::output`].
    registered: AtomicBool,

    /// The read loops with output on its way into the log, see
    /// [`SharedContainerAttach::logging`].
    logging: AtomicUsize,

    /// Notified once no read loop has output on its way into the log.
    logged: Notify,
}

impl Default for SharedContainerAttach {
    fn default() -> Self {
        Self::new(false)
    }
}

impl SharedContainerAttach {
    /// Create a new shared container attach instance for a container with or
    /// without a terminal.
    pub(crate) fn new(terminal: bool) -> Self {
        let (read_half_tx, read_half_rx) = crate::channel::bounded(STDIN_CHANNEL_SIZE);
        Self {
            read_half_rx,
            read_half_tx,
            output: Arc::default(),
            stdin_closed: CancellationToken::new(),
            stop_after_stdin_eof: Arc::default(),
            terminal,
            output_stopped: CancellationToken::new(),
            listeners: Arc::default(),
        }
    }

    /// Add a new attach endpoint to this shared container attach instance. The
    /// `token` of the container stops it on exit.
    pub(crate) fn add(
        &self,
        socket_path: &Path,
        token: CancellationToken,
        stop_after_stdin_eof: bool,
    ) -> Result<()> {
        // The socket gets created only by the first attach, so its connections
        // use the latest setting. CRI-O derives it from the container config,
        // which makes all attaches of a container agree.
        self.stop_after_stdin_eof
            .store(stop_after_stdin_eof, Ordering::Release);
        create_listener(socket_path, self.clone(), token).context("create attach endpoint")
    }

    /// Read from all attach endpoints standard input and return the first result.
    pub(crate) async fn read(&self) -> Result<Bytes> {
        self.read_half_rx
            .recv()
            .await
            .context("receive attach message")
    }

    /// Try to read from all attach endpoints standard input and return the first result.
    pub(crate) fn try_read(&self) -> Result<Bytes> {
        self.read_half_rx
            .try_recv()
            .context("try to receive attach message")
    }

    /// Send data to the container stdin. This waits while the container does
    /// not keep up, and drops the data once nothing reads the stdin anymore.
    pub(crate) async fn send_stdin(&self, data: Bytes) {
        if self.read_half_tx.send(data).await.is_err() {
            trace!("Dropping attach stdin because the container stdin is closed");
        }
    }

    /// Close the stdin channel once nothing reads it anymore, so that senders
    /// stop waiting for it. Queued data can still be read.
    pub(crate) fn close_stdin(&self) {
        self.read_half_rx.close();
    }

    /// Write a buffer to all attach endpoints. Sending fails only if the last
    /// client went away meanwhile, which is no reason to stop the output.
    pub(crate) fn write(&self, m: Message) {
        let sent = self.output.tx.get().is_some_and(|tx| tx.send(m).is_ok());
        if !sent {
            trace!("No attach clients for the output");
        }
    }

    /// Register the output of a container, which ends once the returned value
    /// and all of its clones got dropped. The attach endpoints get the done
    /// message only then, otherwise the done of one pipe could cut off the
    /// remaining output of another one.
    pub(crate) fn output(&self) -> Arc<AttachOutput> {
        self.output.registered.store(true, Ordering::SeqCst);
        Arc::new(AttachOutput {
            output: self.output.clone(),
        })
    }

    /// Whether the container has a terminal.
    pub(crate) fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Stop reading the output once nothing takes it anymore, like once it
    /// got drained after the exit of the process. A process left behind
    /// which holds the pipes or the terminal open would keep the read loops
    /// going forever otherwise, and writing to the log after the exit file
    /// told that it is complete. Closing them lets its writes fail, like
    /// once conmon exited.
    pub(crate) fn stop_output(&self) {
        self.output_stopped.cancel();
    }

    /// Stop the output once the consumer of an exec is done with it and the
    /// process exited, see [`StopOutput`].
    pub(crate) fn stop_output_on_exit(&self, exited: CancellationToken) -> StopOutput {
        StopOutput {
            attach: self.clone(),
            exited,
        }
    }

    /// Whether an output got registered, which read loops did then.
    pub(crate) fn has_output(&self) -> bool {
        self.output.registered.load(Ordering::SeqCst)
    }

    /// The token cancelled by [`SharedContainerAttach::stop_output`].
    pub(crate) fn output_stopped(&self) -> &CancellationToken {
        &self.output_stopped
    }

    /// The token cancelled once an attach client closed the container stdin.
    pub(crate) fn stdin_closed(&self) -> &CancellationToken {
        &self.stdin_closed
    }

    /// Close the container stdin after an attach client reached EOF. Only
    /// that, the container token also drives the exit and OOM handling. A
    /// terminal cannot be closed without ending its output, so it stays open
    /// and this returns false.
    pub(crate) fn close_container_stdin(&self) -> bool {
        if !self.terminal {
            self.stdin_closed.cancel();
        }
        !self.terminal
    }

    /// Forget a listener which stopped on the exit or an error, and remove
    /// its socket if it is still at the path, unless another listener
    /// replaced it meanwhile. The caller keeps the listener open, so that
    /// the inode number of its socket is not reused yet.
    fn remove_listener(&self, path: &Path, id: u64) {
        let mut listeners = lock(&self.listeners);
        let Some(listener) = listeners.remove(path) else {
            return;
        };
        if listener.id != id {
            listeners.insert(path.to_path_buf(), listener);
            return;
        }
        match listener.file.remove_at(path) {
            Ok(()) => {}
            Err(RemoveError::Stat(e)) => {
                debug!("Unable to stat attach socket {}: {e}", path.display());
            }
            Err(RemoveError::Remove(e)) => {
                debug!("Unable to remove attach socket {}: {e}", path.display());
            }
        }
    }

    /// Whether an attach client receives the output.
    #[cfg(test)]
    pub(crate) fn has_readers(&self) -> bool {
        self.receiver_count() > 0
    }

    #[cfg(test)]
    fn receiver_count(&self) -> usize {
        self.output.tx.get().map_or(0, Sender::receiver_count)
    }

    /// Subscribe to the container output sent to the attach endpoints. The
    /// returned flag is true if the output already ended, then the done
    /// message may have been sent before subscribing and the receiver only
    /// gets what is still queued.
    pub(crate) fn subscribe(&self) -> (Receiver<Message>, bool) {
        let rx = self
            .output
            .tx
            .get_or_init(|| broadcast::channel(OUTPUT_CHANNEL_SIZE).0)
            .subscribe();
        // Check only after subscribing, otherwise the done message could be
        // sent in between. Pairs with the fence in `AttachOutput::drop`: of
        // the two fences, the end of the output either passes its fence
        // first, then this sees the end, or last, then it sees the sender and
        // sends the done message after this subscribed.
        fence(Ordering::SeqCst);
        (rx, self.output.ended.load(Ordering::SeqCst))
    }

    /// Mark output which a read loop got as on its way into the log, until
    /// the returned guard gets dropped once the output got broadcast. A slow
    /// log driver delays the output, which is no pause of it, see
    /// [`SharedContainerAttach::wait_logged`].
    pub(crate) fn logging(&self) -> Logging<'_> {
        self.output.logging.fetch_add(1, Ordering::SeqCst);
        Logging(&self.output)
    }

    /// Whether a read loop has output on its way into the log.
    pub(crate) fn is_logging(&self) -> bool {
        self.output.logging.load(Ordering::SeqCst) > 0
    }

    /// Wait until no read loop has output on its way into the log.
    pub(crate) async fn wait_logged(&self) {
        loop {
            let notified = self.output.logged.notified();
            let mut notified = pin!(notified);
            // Before checking, so that a notification in between is not
            // missed.
            notified.as_mut().enable();
            if !self.is_logging() {
                return;
            }
            notified.await;
        }
    }
}

/// Stops the output of an exec once dropped by the consumer of its output,
/// after the process exited, see [`SharedContainerAttach::stop_output`].
/// Without exit files, the consumer decides when the output is complete: a
/// slow one blocks the read loops, which a stop after a pause of the output
/// would cut off with data left in the pipes. A consumer which went away
/// before the exit takes nothing anymore, the output stops with the exit.
pub(crate) struct StopOutput {
    attach: SharedContainerAttach,
    exited: CancellationToken,
}

impl Drop for StopOutput {
    fn drop(&mut self) {
        if self.attach.output_stopped().is_cancelled() {
            return;
        }
        if self.exited.is_cancelled() {
            self.attach.stop_output();
            return;
        }
        // Not waiting forever for an exit which got lost, after which the
        // output got stopped otherwise.
        let attach = self.attach.clone();
        let exited = self.exited.clone();
        task::spawn(async move {
            select! {
                () = exited.cancelled() => attach.stop_output(),
                () = attach.output_stopped().cancelled() => {}
            }
        });
    }
}

/// Output on its way into the log, see [`SharedContainerAttach::logging`].
pub(crate) struct Logging<'a>(&'a OutputState);

impl Drop for Logging<'_> {
    fn drop(&mut self) {
        if self.0.logging.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.logged.notify_waiters();
        }
    }
}

/// The container output registered by `SharedContainerAttach::output`.
#[derive(Debug)]
pub(crate) struct AttachOutput {
    output: Arc<OutputState>,
}

impl Drop for AttachOutput {
    fn drop(&mut self) {
        // Set before sending, so that an attach client which subscribes
        // after the done message still sees that the output ended, see
        // `SharedContainerAttach::subscribe`.
        self.output.ended.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        // Sending fails only without attach clients, which then need no done
        // message.
        if let Some(tx) = self.output.tx.get() {
            let _ = tx.send(Message::Done);
        }
    }
}

/// Removes the entry of a listener and its socket however its task ends.
/// Another listener which replaced it took over the path and the entry with
/// an id of its own, which stay. The listener must still be open, see
/// [`SharedContainerAttach::remove_listener`].
struct ListenerCleanup {
    attach: SharedContainerAttach,
    path: PathBuf,
    id: u64,
}

impl Drop for ListenerCleanup {
    fn drop(&mut self) {
        self.attach.remove_listener(&self.path, self.id);
    }
}

#[cfg(test)]
mod tests;
