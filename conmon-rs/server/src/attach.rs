//! The attach sockets of the containers, see the attach socket section of
//! usage.md: the stdin of the clients towards the container and the container
//! output towards the clients.

use crate::{
    async_fd,
    container_io::{DRAIN_MAX_WAIT, Drain, Lag, Message, Pipe, with_scratch},
    fs_util::{FileId, RemoveError},
    listener,
    mutex::lock,
};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
    sys::socket::{Backlog, SockType, UnixAddr, bind, listen},
};
use std::{
    collections::HashMap,
    fs, future,
    io::{self, IoSlice},
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::FileTypeExt,
    },
    path::{Path, PathBuf},
    pin::{Pin, pin},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence},
    },
    task::{Poll, ready},
};
use tokio::{
    io::{ErrorKind, Interest},
    select,
    sync::{
        Notify,
        broadcast::{self, Receiver, Sender, error::RecvError},
    },
    task, time,
};
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, trace, warn};

/// Capacity of the stdin channel towards the container.
const STDIN_CHANNEL_SIZE: usize = 16;

/// Capacity of the output broadcast channel towards the attach clients. A
/// client which falls further behind than this loses the oldest output,
/// which gets logged with the number of skipped messages, see [`Lag`]. The
/// container output never waits for an attach client, a slow or stuck client
/// must not block the container or the other clients, and the server does
/// not buffer output without bound for it either. About what the stdio
/// channels buffer for exec, as a power of two, which tokio rounds the
/// capacity of a broadcast channel up to anyway.
const OUTPUT_CHANNEL_SIZE: usize = 128;

#[derive(Clone, Debug)]
/// A shared container attach abstraction.
pub struct SharedContainerAttach {
    // A bounded channel instead of a broadcast, so that a slow container stdin
    // applies backpressure to the attach clients instead of dropping input.
    read_half_rx: async_channel::Receiver<Bytes>,
    read_half_tx: async_channel::Sender<Bytes>,

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

#[derive(Debug)]
/// A listening attach socket and the identity of its file, which tells if
/// the file got removed or replaced meanwhile. The inode stays in use while
/// the listener is open, so its number cannot be reused until then.
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
    pub fn new(terminal: bool) -> Self {
        let (read_half_tx, read_half_rx) = async_channel::bounded(STDIN_CHANNEL_SIZE);
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
    pub fn add<T>(
        &self,
        socket_path: T,
        token: CancellationToken,
        stop_after_stdin_eof: bool,
    ) -> Result<()>
    where
        T: AsRef<Path>,
        PathBuf: From<T>,
    {
        // The socket gets created only by the first attach, so its connections
        // use the latest setting. CRI-O derives it from the container config,
        // which makes all attaches of a container agree.
        self.stop_after_stdin_eof
            .store(stop_after_stdin_eof, Ordering::Release);
        Attach::create(socket_path, self.clone(), token).context("create attach endpoint")
    }

    /// Read from all attach endpoints standard input and return the first result.
    pub async fn read(&self) -> Result<Bytes> {
        self.read_half_rx
            .recv()
            .await
            .context("receive attach message")
    }

    /// Try to read from all attach endpoints standard input and return the first result.
    pub fn try_read(&self) -> Result<Bytes> {
        self.read_half_rx
            .try_recv()
            .context("try to receive attach message")
    }

    /// Send data to the container stdin. This waits while the container does
    /// not keep up, and drops the data once nothing reads the stdin anymore.
    pub async fn send_stdin(&self, data: Bytes) {
        if self.read_half_tx.send(data).await.is_err() {
            trace!("Dropping attach stdin because the container stdin is closed");
        }
    }

    /// Close the stdin channel once nothing reads it anymore, so that senders
    /// stop waiting for it. Queued data can still be read.
    pub fn close_stdin(&self) {
        self.read_half_rx.close();
    }

    /// Write a buffer to all attach endpoints. Sending fails only if the last
    /// client went away meanwhile, which is no reason to stop the output.
    pub fn write(&self, m: Message) {
        let sent = self.output.tx.get().is_some_and(|tx| tx.send(m).is_ok());
        if !sent {
            trace!("No attach clients left for the output");
        }
    }

    /// Register the output of a container, which ends once the returned value
    /// and all of its clones got dropped. The attach endpoints get the done
    /// message only then, otherwise the done of one pipe could cut off the
    /// remaining output of another one.
    pub fn output(&self) -> Arc<AttachOutput> {
        self.output.registered.store(true, Ordering::SeqCst);
        Arc::new(AttachOutput {
            output: self.output.clone(),
        })
    }

    /// Whether the container has a terminal.
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Stop reading the output once nothing takes it anymore, like once it
    /// got drained after the exit of the process. A process left behind
    /// which holds the pipes or the terminal open would keep the read loops
    /// going forever otherwise, and writing to the log after the exit file
    /// told that it is complete. Closing them lets its writes fail, like
    /// once conmon exited.
    pub fn stop_output(&self) {
        self.output_stopped.cancel();
    }

    /// Stop the output once the consumer of an exec is done with it and the
    /// process exited, see [`StopOutput`].
    pub fn stop_output_on_exit(&self, exited: CancellationToken) -> StopOutput {
        StopOutput {
            attach: self.clone(),
            exited,
        }
    }

    /// Whether an output got registered, which read loops did then.
    pub fn has_output(&self) -> bool {
        self.output.registered.load(Ordering::SeqCst)
    }

    /// The token cancelled by [`SharedContainerAttach::stop_output`].
    pub fn output_stopped(&self) -> &CancellationToken {
        &self.output_stopped
    }

    /// The token cancelled once an attach client closed the container stdin.
    pub fn stdin_closed(&self) -> &CancellationToken {
        &self.stdin_closed
    }

    /// Close the container stdin after an attach client reached EOF. Only
    /// that, the container token also drives the exit and OOM handling. A
    /// terminal cannot be closed without ending its output, so it stays open
    /// and this returns false.
    pub fn close_container_stdin(&self) -> bool {
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
            Ok(_) => {}
            Err(RemoveError::Stat(e)) => {
                debug!("Unable to stat attach socket {}: {e}", path.display());
            }
            Err(RemoveError::Remove(e)) => {
                debug!("Unable to remove attach socket {}: {e}", path.display());
            }
        }
    }

    /// Check if there are any active attach readers.
    pub fn has_readers(&self) -> bool {
        self.receiver_count() > 0
    }

    fn receiver_count(&self) -> usize {
        self.output.tx.get().map_or(0, Sender::receiver_count)
    }

    /// Subscribe to the container output sent to the attach endpoints. The
    /// returned flag is true if the output already ended, then the done
    /// message may have been sent before subscribing and the receiver only
    /// gets what is still queued.
    pub fn subscribe(&self) -> (Receiver<Message>, bool) {
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
    pub fn logging(&self) -> Logging<'_> {
        self.output.logging.fetch_add(1, Ordering::SeqCst);
        Logging(&self.output)
    }

    /// Whether a read loop has output on its way into the log.
    pub fn is_logging(&self) -> bool {
        self.output.logging.load(Ordering::SeqCst) > 0
    }

    /// Wait until no read loop has output on its way into the log.
    pub async fn wait_logged(&self) {
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
pub struct StopOutput {
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
pub struct Logging<'a>(&'a OutputState);

impl Drop for Logging<'_> {
    fn drop(&mut self) {
        if self.0.logging.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.logged.notify_waiters();
        }
    }
}

#[derive(Debug)]
/// The container output registered by `SharedContainerAttach::output`.
pub struct AttachOutput {
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

/// Check if no more input will arrive after a zero byte read. That is the
/// case once the peer closed its write side, which a SOCK_SEQPACKET socket
/// reports as POLLRDHUP, and no data is queued anymore. Checking the flag
/// first makes sure that no data arrives in between.
#[expect(unsafe_code, reason = "the FIONREAD ioctl")]
pub(crate) fn input_ended(socket: &UnixSeqpacket) -> io::Result<bool> {
    // Besides POLLRDHUP, poll can only report a hangup or an error, which
    // end the reading as well. The number of ready fds tells that, nix
    // does not decode the POLLRDHUP bit of the returned events.
    let events = PollFlags::from_bits_retain(libc::POLLRDHUP);
    let read_closed = loop {
        match poll(
            &mut [PollFd::new(socket.as_fd(), events)],
            PollTimeout::ZERO,
        ) {
            Err(Errno::EINTR) => {}
            res => break res? > 0,
        }
    };
    if !read_closed {
        return Ok(false);
    }

    // For a SOCK_SEQPACKET socket, FIONREAD sums up all queued packets.
    let mut queued: libc::c_int = 0;
    // SAFETY: FIONREAD writes a single c_int.
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::FIONREAD, &mut queued) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(queued == 0)
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

#[derive(Clone, Debug)]
/// Attach handles the attach socket IO of a container.
struct Attach;

impl Attach {
    /// The most data of an output packet, after its pipe byte. Like for
    /// conmon, CRI-O and the Go client read one byte more than this, so that
    /// a full read of a pipe goes out as one packet.
    const PACKET_BUF_SIZE: usize = 8192;

    /// The maximum size of a stdin packet from an attach client, a larger one
    /// gets truncated. The Go client writes at most 32 KiB at once, like
    /// CRI-O does for conmon, plus the start of a detach key sequence which
    /// it held back, a few bytes which the extra KiB leaves room for. The
    /// scratch buffer of the thread keeps this size after the first stdin,
    /// see [`with_scratch`].
    const STDIN_PACKET_MAX_SIZE: usize = 33 * 1024;

    /// The packet indicating that we're done writing.
    const DONE_PACKET: &'static [u8; 1] = &[0];

    /// Create a new attach instance, unless one listens on the path already.
    fn create<T>(
        socket_path: T,
        attach: SharedContainerAttach,
        token: CancellationToken,
    ) -> Result<()>
    where
        T: AsRef<Path>,
        PathBuf: From<T>,
    {
        let path = socket_path.as_ref();

        // Held until the new listener got registered, so that a listener
        // which stops meanwhile does not remove its socket.
        let mut listeners = lock(&attach.listeners);
        if let Some(listener) = listeners.remove(path) {
            if !listener.stop.is_cancelled() && listener.is_bound_at(path) {
                debug!("Already listening on attach socket {}", path.display());
                listeners.insert(path.to_path_buf(), listener);
                return Ok(());
            }
            // Nobody can connect to a socket which got removed, for example by
            // an older client, or replaced. Stop accepting on it, otherwise
            // the listener would wait until the container exits.
            debug!(
                "Attach socket {} got removed or replaced, listening again",
                path.display()
            );
            listener.stop.cancel();
        }

        // A socket of this path which no listener of this server accepts on
        // is left over, for example by a previous server.
        match fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_socket() => {
                debug!("Removing stale attach socket {}", path.display());
                fs::remove_file(path).context("remove stale attach socket")?;
            }
            Ok(_) => bail!(
                "attach socket path {} exists and is no socket",
                path.display()
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("check attach socket path"),
        }

        debug!("Creating attach socket: {}", path.display());
        let fd = listener::private_socket(SockType::SeqPacket).context("create attach socket")?;

        // keep parent_fd in scope until the bind, or else the socket will not work
        let (shortened_path, _parent_dir) = listener::shorten_socket_path(path)?;
        let addr = UnixAddr::new(&shortened_path).context("create socket addr")?;
        bind(fd.as_raw_fd(), &addr).context("bind socket fd")?;

        // Nothing listens on the socket if the setup fails, remove it. The
        // socket stays open until then, so the file is still the own one.
        let remove = |e: anyhow::Error| {
            if let Err(e) = fs::remove_file(path) {
                debug!("Unable to remove attach socket {}: {:#}", path.display(), e);
            }
            e
        };
        let held = fd
            .try_clone()
            .context("duplicate attach socket")
            .map_err(remove)?;
        let setup = || -> Result<(fs::Metadata, UnixSeqpacketListener)> {
            let metadata = path.metadata().context("get attach socket metadata")?;
            listen(&fd, Backlog::MAXCONN).context("listen on socket fd")?;
            let listener =
                UnixSeqpacketListener::try_from(fd).context("create seqpacket listener")?;
            Ok((metadata, listener))
        };
        let (metadata, mut listener) = setup().map_err(remove)?;
        drop(held);

        let stop = CancellationToken::new();
        let id = AttachListener::next_id();
        listeners.insert(
            path.to_path_buf(),
            AttachListener {
                id,
                file: FileId::new(&metadata),
                stop: stop.clone(),
            },
        );
        drop(listeners);

        let cleanup = ListenerCleanup {
            attach,
            path: path.to_path_buf(),
            id,
        };
        task::spawn(
            async move {
                Self::start(&mut listener, &cleanup.path, &cleanup.attach, token, stop).await;
                // The cleanup runs before the listener gets closed.
                drop(cleanup);
                drop(listener);
            }
            .instrument(debug_span!("attach")),
        );

        Ok(())
    }

    /// Accept connections until the container exits, which cancels `token`,
    /// or another listener replaces this one, which cancels `stop`.
    async fn start(
        listener: &mut UnixSeqpacketListener,
        path: &Path,
        attach: &SharedContainerAttach,
        token: CancellationToken,
        stop: CancellationToken,
    ) {
        debug!("Start listening on attach socket {}", path.display());
        let mut accept_errors = listener::AcceptErrors::default();
        loop {
            let accepted = select! {
                accepted = listener.accept() => accepted,
                () = token.cancelled() => {
                    debug!("Stopping attach listener because token cancelled");
                    return;
                }
                () = stop.cancelled() => {
                    debug!("Stopping attach listener because it got replaced");
                    return;
                }
            };
            match accepted {
                Ok(socket) => {
                    debug!("Got new attach stream connection");
                    let read = Arc::new(socket);
                    let write = read.clone();

                    let attach_clone = attach.clone();
                    let token_clone = token.clone();
                    task::spawn(
                        async move {
                            if let Err(e) = Self::read_loop(read, attach_clone, token_clone).await {
                                error!("Attach read loop failure: {e:#}");
                            }
                        }
                        .instrument(debug_span!("read_loop")),
                    );

                    let (write_half_rx, output_ended) = attach.subscribe();
                    let token_clone = token.clone();
                    task::spawn(
                        async move {
                            if let Err(e) =
                                Self::write_loop(write, write_half_rx, output_ended, token_clone)
                                    .await
                            {
                                error!("Attach write loop failure: {e:#}");
                            }
                        }
                        .instrument(debug_span!("write_loop")),
                    );
                }
                Err(e) => accept_errors.failed(&e).await,
            }
        }
    }

    /// Forward the stdin of a client until it closes, the stdin of the
    /// container got closed or the container exited, which cancels `token`.
    async fn read_loop(
        socket: Arc<UnixSeqpacket>,
        attach: SharedContainerAttach,
        token: CancellationToken,
    ) -> Result<()> {
        let stop_after_stdin_eof = attach.stop_after_stdin_eof.load(Ordering::Acquire);
        let stopped = async || {
            select! {
                () = token.cancelled() => debug!("Exiting because token cancelled"),
                () = attach.stdin_closed.cancelled() => debug!("Exiting because stdin got closed"),
            }
        };

        loop {
            // A client which stays connected must not keep the loop going once
            // the container exited or its stdin got closed.
            let read = select! {
                read = Self::recv_stdin(&socket) => read,
                () = stopped() => return Ok(()),
            };

            match read {
                Ok((data, truncated)) if !data.is_empty() => {
                    if truncated {
                        warn!("Truncated attach stdin packet to {} bytes", data.len());
                    }
                    debug!("Read {} stdin bytes from client", data.len());
                    // Waiting for the container to take the data must not
                    // outlive the container or its stdin.
                    select! {
                        () = attach.send_stdin(data) => {}
                        () = stopped() => return Ok(()),
                    }
                }
                Ok(_) => {
                    // A zero byte read on a SOCK_SEQPACKET socket is either an
                    // empty packet or the peer closing its write side, so
                    // check the socket rather than guessing from another read.
                    if !input_ended(&socket).context("check attach socket for more input")? {
                        trace!("Ignoring empty packet");
                        continue;
                    }
                    // The client closed its write half, no more input will
                    // ever arrive on this connection. The write loop keeps
                    // delivering output independently.
                    debug!("Stopping read loop because there is nothing more to read");
                    if stop_after_stdin_eof {
                        attach.close_container_stdin();
                    }
                    return Ok(());
                }
                Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                    // The client closed the connection without reading all of
                    // its output. The error gets reported only once, the
                    // stdin queued before is still readable.
                    debug!("Attach client reset the connection");
                }
                Err(e) => return Err(e).context("read from attach socket"),
            }
        }
    }

    /// Receive a stdin packet into the scratch buffer of the thread, see
    /// [`with_scratch`], and copy it out, so that a connection holds no
    /// buffer while it waits and the queued stdin only the data of the
    /// packets. Stdin is binary, null bytes are part of the data. Returns
    /// whether the packet got truncated as well.
    async fn recv_stdin(socket: &UnixSeqpacket) -> io::Result<(Bytes, bool)> {
        future::poll_fn(|cx| {
            with_scratch(Self::STDIN_PACKET_MAX_SIZE, |buf| {
                let info = ready!(socket.poll_recv(cx, buf))?;
                let data = Bytes::copy_from_slice(&buf[..info.bytes_read()]);
                Poll::Ready(Ok((data, info.truncated())))
            })
        })
        .await
    }

    /// Whether a send failed because the client closed its end. A client
    /// which leaves output unread gets the connection reset.
    fn client_gone(e: &io::Error) -> bool {
        matches!(e.kind(), ErrorKind::BrokenPipe | ErrorKind::ConnectionReset)
    }

    /// Forward the container output to a client until the output ended or
    /// the client went away. Once the container exited, which cancels
    /// `token`, a process left behind may keep the output open, so forward
    /// only what arrives shortly after the exit.
    async fn write_loop(
        socket: Arc<UnixSeqpacket>,
        mut rx: Receiver<Message>,
        output_ended: bool,
        token: CancellationToken,
    ) -> Result<()> {
        let hangup = Self::hangup(&socket);
        tokio::pin!(hangup);
        let mut drain: Option<Drain> = None;
        let mut lag = Lag::default();
        loop {
            // The done message may have been sent before subscribing, so write
            // it once nothing is queued anymore.
            let message = if output_ended {
                rx.try_recv().unwrap_or(Message::Done)
            } else {
                let res = select! {
                    // The exit starts the drain once, also while the output
                    // keeps coming.
                    biased;
                    () = token.cancelled(), if drain.is_none() => {
                        debug!("Draining the output because token cancelled");
                        drain = Some(Drain::default());
                        continue;
                    }
                    () = &mut hangup => {
                        debug!("Exiting because the client hung up");
                        return Ok(());
                    }
                    res = Self::next_message(&mut rx, drain.as_mut(), &mut lag) => res,
                };
                match res {
                    Ok(message) => message,
                    Err(RecvError::Lagged(n)) => {
                        lag.skipped(n);
                        continue;
                    }
                    Err(RecvError::Closed) => {
                        debug!("Exiting because container I/O is gone");
                        return Ok(());
                    }
                }
            };
            let draining = drain.is_some();
            match message {
                Message::Done => {
                    debug!("Exiting because the container output ended");
                    let packet = [IoSlice::new(Self::DONE_PACKET)];
                    match Self::send(&socket, &packet, hangup.as_mut(), &token, draining).await {
                        Ok(true) => debug!("Wrote done packet to client"),
                        Ok(false) => {}
                        Err(e) => bail!("unable to write done packet: {e:#}"),
                    }
                    return Ok(());
                }
                Message::Data(buf, pipe) => {
                    let p = match pipe {
                        Pipe::StdOut => 2,
                        Pipe::StdErr => 3,
                    };
                    let header = [p];
                    let len = buf.chunks(Self::PACKET_BUF_SIZE).len();
                    for (idx, chunk) in buf.chunks(Self::PACKET_BUF_SIZE).enumerate() {
                        // Send the pipe header and the chunk as one packet
                        // without copying the chunk into a separate buffer.
                        let packet = [IoSlice::new(&header), IoSlice::new(chunk)];
                        match Self::send(&socket, &packet, hangup.as_mut(), &token, draining).await
                        {
                            Ok(true) => {
                                debug!("Wrote {} packet {}/{} to client", pipe, idx + 1, len);
                            }
                            // The client is gone, so stop receiving output for
                            // it as well.
                            Ok(false) => return Ok(()),
                            Err(e) => bail!("unable to write packet {}/{}: {:#}", idx + 1, len, e),
                        }
                    }
                }
            }
        }
    }

    /// Send a packet, false if the client went away or does not take it in
    /// time. A client which does not read keeps a send waiting, until it
    /// hangs up or, once the container exited, for at most
    /// [`DRAIN_MAX_WAIT`]. Each packet gets that long, so that a slow client
    /// still gets the output which was there already.
    async fn send(
        socket: &UnixSeqpacket,
        packet: &[IoSlice<'_>],
        hangup: Pin<&mut impl Future<Output = ()>>,
        token: &CancellationToken,
        draining: bool,
    ) -> io::Result<bool> {
        let stuck = async {
            if !draining {
                token.cancelled().await;
            }
            time::sleep(DRAIN_MAX_WAIT).await;
        };
        select! {
            res = socket.send_vectored(packet) => match res {
                Ok(_) => Ok(true),
                Err(ref e) if Self::client_gone(e) => {
                    debug!("Exiting because the client closed the connection");
                    Ok(false)
                }
                Err(e) => Err(e),
            },
            () = hangup => {
                debug!("Exiting because the client hung up");
                Ok(false)
            }
            () = stuck => {
                debug!("Exiting because the client does not take the output after the exit");
                Ok(false)
            }
        }
    }

    /// The next output message, bounded by the drain once the container
    /// exited. If the output did not end after the exit, the client gets
    /// the done packet anyway.
    async fn next_message(
        rx: &mut Receiver<Message>,
        drain: Option<&mut Drain>,
        lag: &mut Lag,
    ) -> Result<Message, RecvError> {
        match drain {
            Some(drain) => Ok(drain
                .recv_output(rx, Some(lag))
                .await
                .unwrap_or(Message::Done)),
            None => {
                let res = rx.recv().await;
                if res.is_ok() {
                    lag.received(rx);
                }
                res
            }
        }
    }

    /// Wait until the client closed the connection. Without output, a send
    /// never notices, and a client which only closed its write side still
    /// receives the output. The hangup gets reported by an epoll instance of
    /// its own, which watches the socket for nothing but the hangups and
    /// errors epoll always reports. Neither input nor output wakes it up,
    /// and its readiness is separate from the one of the socket, which the
    /// read and write loops use. If that fails, only a failing send ends
    /// the loop.
    async fn hangup(socket: &UnixSeqpacket) {
        let wait = async || -> Result<()> {
            let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).context("create epoll")?;
            epoll
                .add(socket.as_fd(), EpollEvent::new(EpollFlags::EPOLLET, 0))
                .context("watch attach socket")?;
            let ready = async_fd::register(epoll.0.as_fd(), Interest::READABLE)
                .context("register epoll")?;
            let mut events = [EpollEvent::empty()];
            loop {
                let mut guard = ready.readable().await.context("wait for epoll")?;
                let n = epoll
                    .wait(&mut events, PollTimeout::ZERO)
                    .context("read epoll events")?;
                if n > 0 {
                    return Ok(());
                }
                guard.clear_ready();
            }
        };
        if let Err(e) = wait().await {
            warn!("Unable to watch for the attach client to hang up: {e:#}");
            future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests;
