use crate::{
    async_fd,
    container_io::{DRAIN_MAX_WAIT, Drain, Message, Pipe},
    listener,
    mutex::lock,
};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
    sys::socket::{AddressFamily, Backlog, SockFlag, SockType, UnixAddr, bind, listen, socket},
};
use std::{
    collections::HashMap,
    fs, future,
    io::{self, IoSlice},
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering, fence},
    },
};
use tokio::{
    io::{ErrorKind, Interest},
    select,
    sync::broadcast::{self, Receiver, Sender, error::RecvError},
    task, time,
};
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, trace, warn};

/// Capacity of the stdin channel towards the container.
const STDIN_CHANNEL_SIZE: usize = 16;

/// Capacity of the output broadcast channel towards the attach clients. A
/// client which falls further behind than this loses the oldest output,
/// which gets logged with the number of skipped messages. The container
/// output never waits for an attach client, a slow or stuck client must not
/// block the container or the other clients, and the server does not buffer
/// output without bound for it either. It matches what the stdio channels
/// buffer for exec.
const OUTPUT_CHANNEL_SIZE: usize = 100;

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
    dev: u64,
    ino: u64,

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
        Self::is_file(path, self.dev, self.ino)
    }

    fn is_file(path: &Path, dev: u64, ino: u64) -> bool {
        fs::symlink_metadata(path).is_ok_and(|m| m.dev() == dev && m.ino() == ino)
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
        Arc::new(AttachOutput {
            output: self.output.clone(),
        })
    }

    /// Whether the container has a terminal.
    pub fn is_terminal(&self) -> bool {
        self.terminal
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
        let mut listeners = match lock(&self.listeners) {
            Ok(listeners) => listeners,
            Err(e) => {
                warn!("Unable to lock attach listeners: {e:#}");
                return;
            }
        };
        let Some(listener) = listeners.remove(path) else {
            return;
        };
        if listener.id != id {
            listeners.insert(path.to_path_buf(), listener);
            return;
        }
        if listener.is_bound_at(path)
            && let Err(e) = fs::remove_file(path)
        {
            debug!("Unable to remove attach socket {}: {e}", path.display());
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
    /// The size of an attach packet.
    const PACKET_BUF_SIZE: usize = 8192;

    /// The maximum size of a stdin packet from an attach client, a larger one
    /// gets truncated. The Go client writes at most 32 KiB at once.
    const STDIN_PACKET_MAX_SIZE: usize = 64 * 1024;

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
        let mut listeners = lock(&attach.listeners)?;
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
        let fd = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
            None,
        )
        .context("bind socket")?;

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
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(path, permissions).context("set attach socket permissions")?;
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
                dev: metadata.dev(),
                ino: metadata.ino(),
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
                Err(e) => listener::accept_failed(&e).await,
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

        // Every read returns a single packet, which gets copied out so that
        // the queued stdin holds only the data of the packets.
        let mut buf = vec![0; Self::STDIN_PACKET_MAX_SIZE];
        loop {
            // In situations we're processing output directly from the I/O streams
            // we need a mechanism to figure out when to stop that doesn't involve reading the
            // number of bytes read.
            // Thus, we need to select on the cancellation token saved in the child.
            // While this could result in a data race, as select statements are racy,
            // we won't interleve these two futures, as one ends execution.
            let read = select! {
                read = socket.recv(&mut buf) => read,
                () = stopped() => return Ok(()),
            };

            match read {
                Ok(info) if info.bytes_read() > 0 => {
                    let n = info.bytes_read();
                    if info.truncated() {
                        warn!("Truncated attach stdin packet to {n} bytes");
                    }
                    // Stdin is binary, null bytes are part of the data.
                    let data = Bytes::copy_from_slice(&buf[..n]);
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
                    if !Self::input_ended(&socket).context("check attach socket for more input")? {
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
                Err(e) => match Errno::from_raw(e.raw_os_error().context("get OS error")?) {
                    Errno::ECONNRESET => {
                        // The client closed the connection without reading all
                        // of its output. The error gets reported only once,
                        // the stdin queued before is still readable.
                        debug!("Attach client reset the connection");
                        continue;
                    }
                    Errno::EIO => {
                        debug!("Stopping read loop because of IO error");
                        return Ok(());
                    }
                    Errno::EBADF => return Err(e).context("attach socket closed"),
                    _ => return Err(e).context("read from attach socket"),
                },
            }
        }
    }

    /// Check if no more input will arrive after a zero byte read. That is the
    /// case once the peer closed its write side, which a SOCK_SEQPACKET socket
    /// reports as POLLRDHUP, and no data is queued anymore. Checking the flag
    /// first makes sure that no data arrives in between.
    #[expect(unsafe_code, reason = "the FIONREAD ioctl")]
    fn input_ended(socket: &UnixSeqpacket) -> io::Result<bool> {
        // Besides POLLRDHUP, poll can only report a hangup or an error, which
        // end the reading as well. The number of ready fds tells that, nix
        // does not decode the POLLRDHUP bit of the returned events.
        let events = PollFlags::from_bits_retain(libc::POLLRDHUP);
        let read_closed = loop {
            match poll(
                &mut [PollFd::new(socket.as_fd(), events)],
                PollTimeout::ZERO,
            ) {
                Err(Errno::EINTR) => continue,
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
                    res = Self::next_message(&mut rx, drain.as_mut()) => res,
                };
                match res {
                    Ok(message) => message,
                    Err(RecvError::Lagged(n)) => {
                        warn!("Attach client too slow, skipped {n} messages");
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
                    let len = buf.chunks(Self::PACKET_BUF_SIZE - 1).len();
                    for (idx, chunk) in buf.chunks(Self::PACKET_BUF_SIZE - 1).enumerate() {
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
    ) -> Result<Message, RecvError> {
        match drain {
            Some(drain) => Ok(drain.recv_output(rx, true).await.unwrap_or(Message::Done)),
            None => rx.recv().await,
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
mod tests {
    use super::*;
    use crate::{
        container_io::{ContainerIO, StdioSender},
        container_log::ContainerLog,
    };
    use nix::sys::socket::{MsgFlags, connect, send};
    use std::os::fd::OwnedFd;
    use std::{
        io,
        net::Shutdown,
        pin::Pin,
        task::{Context as TaskContext, Poll},
    };
    use tokio::{
        io::{AsyncRead, ReadBuf},
        task::JoinHandle,
        time::{Duration, timeout},
    };

    /// A connected pair of packet sockets, like the attach clients use. The
    /// first one is the server side.
    fn seqpacket_pair() -> Result<(Arc<UnixSeqpacket>, UnixSeqpacket)> {
        let (server, client) = UnixSeqpacket::pair()?;
        Ok((Arc::new(server), client))
    }

    /// Spawn a read loop for the server side of a packet socket pair and
    /// return the client side.
    fn spawn_read_loop(
        attach: &SharedContainerAttach,
        token: &CancellationToken,
    ) -> Result<(UnixSeqpacket, JoinHandle<Result<()>>)> {
        let (server, client) = seqpacket_pair()?;
        let task = task::spawn(Attach::read_loop(server, attach.clone(), token.clone()));
        Ok((client, task))
    }

    struct BadFd;

    impl AsyncRead for BadFd {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::from_raw_os_error(Errno::EBADF as i32)))
        }
    }

    #[tokio::test]
    async fn done_after_all_pipes() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let (mut rx, _) = attach.subscribe();

        let stdout = attach.output();
        let stderr = stdout.clone();

        drop(stderr);
        attach.write(Message::Data(Bytes::from_static(b"last"), Pipe::StdOut));
        drop(stdout);

        assert!(matches!(rx.recv().await?, Message::Data(..)));
        assert_eq!(rx.recv().await?, Message::Done);
        assert!(rx.try_recv().is_err());
        Ok(())
    }

    #[tokio::test]
    async fn done_after_failed_read_loop() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let (mut rx, _) = attach.subscribe();
        let (message_tx, _message_rx) = async_channel::unbounded();

        let stdout = attach.output();
        let stderr = stdout.clone();

        let res = ContainerIO::read_loop(
            BadFd,
            Pipe::StdErr,
            ContainerLog::new(),
            Some(StdioSender::new(message_tx.clone(), &Arc::default())),
            attach.clone(),
            stderr,
        )
        .await;
        assert!(res.is_err());
        assert!(rx.try_recv().is_err());

        ContainerIO::read_loop(
            tokio::io::empty(),
            Pipe::StdOut,
            ContainerLog::new(),
            Some(StdioSender::new(message_tx, &Arc::default())),
            attach,
            stdout,
        )
        .await?;

        assert_eq!(rx.recv().await?, Message::Done);
        assert!(rx.try_recv().is_err());
        Ok(())
    }

    /// An attach client which subscribes while the output ends either gets
    /// the done message or learns that the output ended.
    #[test]
    fn subscribe_racing_the_end_sees_it() -> Result<()> {
        for _ in 0..2000 {
            let attach = SharedContainerAttach::default();
            let output = attach.output();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let subscriber = std::thread::spawn({
                let (attach, barrier) = (attach.clone(), barrier.clone());
                move || {
                    barrier.wait();
                    attach.subscribe()
                }
            });
            barrier.wait();
            drop(output);
            let (mut rx, ended) = subscriber
                .join()
                .map_err(|_| anyhow::format_err!("subscriber panicked"))?;
            assert!(ended || rx.try_recv() == Ok(Message::Done));
        }
        Ok(())
    }

    #[tokio::test]
    async fn done_after_output_ended() -> Result<()> {
        let attach = SharedContainerAttach::default();
        drop(attach.output());

        let (server, client) = seqpacket_pair()?;
        let (rx, output_ended) = attach.subscribe();
        assert!(output_ended);
        timeout(
            Duration::from_secs(5),
            Attach::write_loop(server, rx, output_ended, CancellationToken::new()),
        )
        .await??;

        let mut buf = [1];
        assert_eq!(client.recv(&mut buf).await?.bytes_read(), 1);
        assert_eq!(&buf, Attach::DONE_PACKET);
        Ok(())
    }

    #[tokio::test]
    async fn write_loop_stops_on_broken_pipe() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let (server, client) = seqpacket_pair()?;
        drop(client);

        let (rx, _) = attach.subscribe();
        let mut write_loop = task::spawn(Attach::write_loop(
            server,
            rx,
            false,
            CancellationToken::new(),
        ));
        // A process forked by a parallel test may hold the client end until
        // it execs, a send succeeds then. Keep sending until one fails.
        for _ in 0..500 {
            attach.write(Message::Data(Bytes::from_static(b"data"), Pipe::StdOut));
            if let Ok(res) = timeout(Duration::from_millis(10), &mut write_loop).await {
                return res?;
            }
        }
        bail!("write loop did not stop")
    }

    /// Receive a packet of the attach protocol.
    async fn recv_packet(client: &UnixSeqpacket) -> Result<Vec<u8>> {
        let mut buf = vec![0; Attach::PACKET_BUF_SIZE];
        let n = timeout(Duration::from_secs(5), client.recv(&mut buf))
            .await??
            .bytes_read();
        buf.truncate(n);
        Ok(buf)
    }

    #[tokio::test]
    async fn write_loop_stops_on_hangup() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let (server, client) = seqpacket_pair()?;
        let (rx, _) = attach.subscribe();
        let write_loop = task::spawn(Attach::write_loop(
            server.clone(),
            rx,
            false,
            CancellationToken::new(),
        ));

        // Closing only the write side keeps the output flowing.
        client.shutdown(Shutdown::Write)?;
        attach.write(Message::Data(Bytes::from_static(b"data"), Pipe::StdOut));
        assert_eq!(recv_packet(&client).await?, b"\x02data");
        assert!(!write_loop.is_finished());

        // A process forked by a parallel test may hold the client end until
        // it execs, which delays the hangup.
        drop(client);
        timeout(Duration::from_secs(5), write_loop).await???;
        assert_eq!(attach.receiver_count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn write_loop_drains_after_exit() -> Result<()> {
        let attach = SharedContainerAttach::default();
        // Output which never ends, like one held open by a process the
        // container left behind.
        let _output = attach.output();
        let (server, client) = seqpacket_pair()?;
        let (rx, _) = attach.subscribe();
        let token = CancellationToken::new();
        let write_loop = task::spawn(Attach::write_loop(server, rx, false, token.clone()));

        attach.write(Message::Data(Bytes::from_static(b"out"), Pipe::StdOut));
        token.cancel();
        attach.write(Message::Data(Bytes::from_static(b"err"), Pipe::StdErr));

        assert_eq!(recv_packet(&client).await?, b"\x02out");
        assert_eq!(recv_packet(&client).await?, b"\x03err");
        assert_eq!(recv_packet(&client).await?, Attach::DONE_PACKET);
        timeout(Duration::from_secs(5), write_loop).await???;
        Ok(())
    }

    #[tokio::test]
    async fn write_loop_gives_up_on_stuck_client_after_exit() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let _output = attach.output();
        let (server, _client) = seqpacket_pair()?;
        let (rx, _) = attach.subscribe();
        let token = CancellationToken::new();
        let write_loop = task::spawn(Attach::write_loop(server, rx, false, token.clone()));

        // A client which does not read, until its socket takes no more.
        let data = Bytes::from(vec![0; Attach::PACKET_BUF_SIZE - 1]);
        for _ in 0..OUTPUT_CHANNEL_SIZE {
            attach.write(Message::Data(data.clone(), Pipe::StdOut));
            task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!write_loop.is_finished());

        // Neither the output nor the done packet wait for it after the exit.
        token.cancel();
        timeout(Duration::from_secs(5), write_loop).await???;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_loop_drains_after_exit_with_busy_output() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let _output = attach.output();
        let (server, client) = seqpacket_pair()?;
        let (rx, _) = attach.subscribe();
        let token = CancellationToken::new();

        // A process left behind which never pauses.
        let stop = Arc::new(AtomicBool::new(false));
        let writer = std::thread::spawn({
            let (attach, stop) = (attach.clone(), stop.clone());
            move || {
                while !stop.load(Ordering::Relaxed) {
                    attach.write(Message::Data(Bytes::from_static(b"x"), Pipe::StdOut));
                }
            }
        });
        let write_loop = task::spawn(Attach::write_loop(server, rx, false, token.clone()));
        tokio::time::sleep(Duration::from_millis(10)).await;
        token.cancel();

        let res = timeout(Duration::from_secs(10), async {
            while recv_packet(&client).await? != Attach::DONE_PACKET {}
            Ok::<_, anyhow::Error>(())
        })
        .await;
        stop.store(true, Ordering::Relaxed);
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("writer panicked"))?;
        res??;
        timeout(Duration::from_secs(5), write_loop).await???;
        Ok(())
    }

    #[tokio::test]
    async fn send_stdin_after_close() -> Result<()> {
        let attach = SharedContainerAttach::default();
        ContainerIO::read_loop_stdin(tokio::io::sink(), attach.clone(), {
            let token = CancellationToken::new();
            token.cancel();
            token
        })
        .await?;

        // More packets than the channel capacity, which get dropped instead of
        // waiting for a reader forever.
        for _ in 0..64 {
            timeout(
                Duration::from_secs(5),
                attach.send_stdin(Bytes::from_static(b"data")),
            )
            .await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn read_loop_forwards_packets_with_null_bytes() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let token = CancellationToken::new();
        let (client, task) = spawn_read_loop(&attach, &token)?;

        for packet in [&b"a\0b"[..], b"\0", b"c"] {
            client.send(packet).await?;
        }

        for expected in [&b"a\0b"[..], b"\0", b"c"] {
            let data = timeout(Duration::from_secs(5), attach.read()).await??;
            assert_eq!(data, expected);
        }

        token.cancel();
        task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn read_loop_forwards_large_packets() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let token = CancellationToken::new();
        let (client, task) = spawn_read_loop(&attach, &token)?;

        let packets = [
            vec![1],
            vec![2; Attach::PACKET_BUF_SIZE],
            vec![3; 4 * Attach::PACKET_BUF_SIZE],
            vec![4; Attach::STDIN_PACKET_MAX_SIZE],
        ];
        for packet in &packets {
            client.send(packet).await?;
        }
        for expected in &packets {
            let data = timeout(Duration::from_secs(5), attach.read()).await??;
            assert_eq!(data, expected);
        }

        // Larger packets get truncated.
        client
            .send(&vec![5; Attach::STDIN_PACKET_MAX_SIZE + 1])
            .await?;
        let data = timeout(Duration::from_secs(5), attach.read()).await??;
        assert_eq!(data, vec![5; Attach::STDIN_PACKET_MAX_SIZE]);

        token.cancel();
        task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn read_loop_forwards_data_after_empty_packets() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let token = CancellationToken::new();
        let (client, task) = spawn_read_loop(&attach, &token)?;

        // All of it is queued before the read loop runs, so the client closed
        // its write side already when the empty packets get read.
        for packet in [&b""[..], b"z", b"", b"y"] {
            client.send(packet).await?;
        }
        client.shutdown(Shutdown::Write)?;

        for expected in [&b"z"[..], b"y"] {
            let data = timeout(Duration::from_secs(5), attach.read()).await??;
            assert_eq!(data, expected);
        }
        timeout(Duration::from_secs(5), task).await???;
        Ok(())
    }

    #[tokio::test]
    async fn read_loop_continues_after_empty_packet() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let token = CancellationToken::new();
        let (client, task) = spawn_read_loop(&attach, &token)?;

        client.send(b"").await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!task.is_finished());

        client.send(b"a").await?;
        let data = timeout(Duration::from_secs(5), attach.read()).await??;
        assert_eq!(data, &b"a"[..]);
        assert!(!task.is_finished());

        token.cancel();
        task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn stdin_eof_keeps_listener() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("attach");
        let token = CancellationToken::new();
        let attach = SharedContainerAttach::default();
        attach.add(&path, token.clone(), true)?;

        let connect_client = || -> Result<OwnedFd> {
            let fd = socket(
                AddressFamily::Unix,
                SockType::SeqPacket,
                SockFlag::SOCK_CLOEXEC,
                None,
            )?;
            connect(fd.as_raw_fd(), &UnixAddr::new(&path)?)?;
            Ok(fd)
        };

        // A client which stops after stdin EOF closes the stdin of the
        // container, but the container keeps running. All of its input is
        // queued before the connection gets accepted.
        let first = connect_client()?;
        for packet in [&b""[..], b"x"] {
            send(first.as_raw_fd(), packet, MsgFlags::empty())?;
        }
        nix::sys::socket::shutdown(first.as_raw_fd(), nix::sys::socket::Shutdown::Write)?;
        let data = timeout(Duration::from_secs(5), attach.read()).await??;
        assert_eq!(data, &b"x"[..]);
        timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
        assert!(!token.is_cancelled());

        // Later clients still get attached.
        let _second = connect_client()?;

        // The exit stops the listener and removes the socket.
        token.cancel();
        timeout(Duration::from_secs(5), async {
            while path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    }

    /// Wait until the condition holds.
    async fn wait_for(mut condition: impl FnMut() -> bool) -> Result<()> {
        timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    }

    /// Connect to an attach socket and wait until the connection receives
    /// the output.
    async fn attach_client(attach: &SharedContainerAttach, path: &Path) -> Result<UnixSeqpacket> {
        let readers = attach.receiver_count();
        let client = UnixSeqpacket::connect(path).await?;
        wait_for(|| attach.receiver_count() > readers).await?;
        Ok(client)
    }

    #[tokio::test]
    async fn listens_again_once_the_socket_got_removed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("attach");
        let token = CancellationToken::new();
        let attach = SharedContainerAttach::default();
        attach.add(&path, token.clone(), false)?;
        let ino = fs::metadata(&path)?.ino();
        let first_stop = lock(&attach.listeners)?
            .get(&path)
            .context("no listener")?
            .stop
            .clone();

        // Another attach keeps the listener.
        attach.add(&path, token.clone(), false)?;
        assert_eq!(fs::metadata(&path)?.ino(), ino);
        assert!(!first_stop.is_cancelled());
        let first = attach_client(&attach, &path).await?;

        // Older clients removed the socket once they were done, nobody could
        // connect to the listener anymore.
        fs::remove_file(&path)?;
        attach.add(&path, token.clone(), false)?;
        assert!(first_stop.is_cancelled());
        assert_eq!(lock(&attach.listeners)?.len(), 1);
        let second = attach_client(&attach, &path).await?;

        // The connections of the stopped listener stay.
        attach.write(Message::Data(Bytes::from_static(b"x"), Pipe::StdOut));
        assert_eq!(recv_packet(&first).await?, b"\x02x");
        assert_eq!(recv_packet(&second).await?, b"\x02x");

        // The stopped listener leaves the new socket alone, the exit removes
        // it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(path.exists());
        token.cancel();
        wait_for(|| !path.exists()).await?;
        wait_for(|| lock(&attach.listeners).is_ok_and(|l| l.is_empty())).await?;
        Ok(())
    }

    #[tokio::test]
    async fn ended_listener_removes_its_entry_and_socket() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("attach");
        let attach = SharedContainerAttach::default();
        attach.add(&path, CancellationToken::new(), false)?;
        let id = lock(&attach.listeners)?
            .get(&path)
            .context("no listener")?
            .id;
        let cleanup = |id| ListenerCleanup {
            attach: attach.clone(),
            path: path.clone(),
            id,
        };

        // One which got replaced, so of another id, leaves them alone.
        drop(cleanup(id + 1));
        assert!(lock(&attach.listeners)?.contains_key(&path));
        assert!(path.exists());

        // Any other end of the task removes both, so that the next attach
        // listens again instead of taking the dead socket for a live one.
        drop(cleanup(id));
        assert!(lock(&attach.listeners)?.is_empty());
        assert!(!path.exists());
        attach.add(&path, CancellationToken::new(), false)?;
        attach_client(&attach, &path).await?;
        Ok(())
    }

    #[tokio::test]
    async fn replaces_a_stale_socket() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("attach");
        let token = CancellationToken::new();

        // A socket left behind by a previous server.
        drop(std::os::unix::net::UnixListener::bind(&path)?);
        let attach = SharedContainerAttach::default();
        attach.add(&path, token.clone(), false)?;
        attach_client(&attach, &path).await?;
        token.cancel();
        wait_for(|| !path.exists()).await?;

        // Anything else stays.
        fs::write(&path, b"")?;
        let attach = SharedContainerAttach::default();
        assert!(attach.add(&path, CancellationToken::new(), false).is_err());
        assert!(path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn read_loop_stops_while_waiting_for_stdin() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let token = CancellationToken::new();
        let (client, read_loop) = spawn_read_loop(&attach, &token)?;

        // Nothing reads the stdin, so the read loop ends up waiting to send.
        for i in 0..=STDIN_CHANNEL_SIZE {
            client.send(&[u8::try_from(i)? + 1]).await?;
        }
        timeout(Duration::from_secs(5), async {
            while attach.read_half_tx.len() < STDIN_CHANNEL_SIZE {
                task::yield_now().await;
            }
        })
        .await?;

        token.cancel();
        timeout(Duration::from_secs(5), read_loop).await???;
        Ok(())
    }

    #[tokio::test]
    async fn read_loop_stops_on_eof() -> Result<()> {
        for (terminal, stop_after_stdin_eof) in [(false, false), (false, true), (true, true)] {
            let attach = SharedContainerAttach::new(terminal);
            attach
                .stop_after_stdin_eof
                .store(stop_after_stdin_eof, Ordering::Release);

            let (server, client) = seqpacket_pair()?;
            drop(client);

            timeout(
                Duration::from_secs(5),
                Attach::read_loop(server, attach.clone(), CancellationToken::new()),
            )
            .await??;
            assert_eq!(
                attach.stdin_closed.is_cancelled(),
                stop_after_stdin_eof && !terminal
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn read_loop_forwards_stdin_after_reset() -> Result<()> {
        let attach = SharedContainerAttach::default();
        attach.stop_after_stdin_eof.store(true, Ordering::Release);
        let (server, client) = seqpacket_pair()?;

        // Closing a socket with unread data resets the connection, which the
        // next read reports before the queued stdin.
        server.send(b"output").await?;
        client.send(b"x").await?;
        drop(client);

        let task = task::spawn(Attach::read_loop(
            server,
            attach.clone(),
            CancellationToken::new(),
        ));
        let data = timeout(Duration::from_secs(5), attach.read()).await??;
        assert_eq!(data, &b"x"[..]);
        timeout(Duration::from_secs(5), task).await???;
        assert!(attach.stdin_closed.is_cancelled());
        Ok(())
    }

    #[tokio::test]
    async fn stdin_without_dropping() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let (client, read_loop) = spawn_read_loop(&attach, &CancellationToken::new())?;

        // More packets than the channel capacity, which have to wait for the
        // reader instead of getting dropped.
        const PACKETS: u8 = 64;
        for i in 0..PACKETS {
            client.send(&[i + 1]).await?;
        }
        drop(client);

        let mut received = vec![];
        while let Ok(data) = timeout(Duration::from_secs(5), attach.read()).await? {
            received.extend_from_slice(&data);
            if received.len() == usize::from(PACKETS) {
                break;
            }
        }
        assert_eq!(received, (1..=PACKETS).collect::<Vec<_>>());

        read_loop.await??;
        Ok(())
    }
}
