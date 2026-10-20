use crate::{
    container_io::{Message, Pipe},
    listener::{self, DefaultListener, Listener},
};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::socket::{AddressFamily, Backlog, SockFlag, SockType, UnixAddr, bind, listen, socket},
};
use std::{
    io::{self, IoSlice},
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::fs::PermissionsExt,
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::ErrorKind,
    select,
    sync::broadcast::{self, Receiver, Sender, error::RecvError},
    task,
};
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, trace, warn};

/// Capacity of the stdin channel towards the container.
const STDIN_CHANNEL_SIZE: usize = 16;

/// Capacity of the output broadcast channel towards the attach clients. A
/// client which falls further behind than this loses output, so it matches
/// what the stdio channels buffer for exec.
const OUTPUT_CHANNEL_SIZE: usize = 100;

#[derive(Clone, Debug)]
/// A shared container attach abstraction.
pub struct SharedContainerAttach {
    // A bounded channel instead of a broadcast, so that a slow container stdin
    // applies backpressure to the attach clients instead of dropping input.
    read_half_rx: async_channel::Receiver<Bytes>,
    read_half_tx: async_channel::Sender<Bytes>,
    write_half_tx: Sender<Message>,
    output_ended: CancellationToken,

    /// Cancelled once a client with `stop_after_stdin_eof` closed its stdin,
    /// which closes the stdin of the container for good. It is separate from
    /// the token of the container, which only the exit may cancel.
    stdin_closed: CancellationToken,
    stop_after_stdin_eof: Arc<AtomicBool>,
    terminal: bool,
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
        let (write_half_tx, _) = broadcast::channel(OUTPUT_CHANNEL_SIZE);
        Self {
            read_half_rx,
            read_half_tx,
            write_half_tx,
            output_ended: CancellationToken::new(),
            stdin_closed: CancellationToken::new(),
            stop_after_stdin_eof: Arc::default(),
            terminal,
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
    pub async fn read(&mut self) -> Result<Bytes> {
        self.read_half_rx
            .recv()
            .await
            .context("receive attach message")
    }

    /// Try to read from all attach endpoints standard input and return the first result.
    pub fn try_read(&mut self) -> Result<Bytes> {
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
        if self.write_half_tx.send(m).is_err() {
            trace!("No attach clients left for the output");
        }
    }

    /// Register the output of a container, which ends once the returned value
    /// and all of its clones got dropped. The attach endpoints get the done
    /// message only then, otherwise the done of one pipe could cut off the
    /// remaining output of another one.
    pub fn output(&self) -> Arc<AttachOutput> {
        Arc::new(AttachOutput {
            write_half_tx: self.write_half_tx.clone(),
            output_ended: self.output_ended.clone(),
        })
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

    /// Check if there are any active attach readers.
    pub fn has_readers(&self) -> bool {
        self.write_half_tx.receiver_count() > 0
    }

    /// Subscribe to the container output sent to the attach endpoints. The
    /// returned flag is true if the output already ended, then the done
    /// message may have been sent before subscribing and the receiver only
    /// gets what is still queued.
    pub fn subscribe(&self) -> (Receiver<Message>, bool) {
        let rx = self.write_half_tx.subscribe();
        // Check only after subscribing, otherwise the done message could be
        // sent in between.
        (rx, self.output_ended.is_cancelled())
    }
}

#[derive(Debug)]
/// The container output registered by `SharedContainerAttach::output`.
pub struct AttachOutput {
    write_half_tx: Sender<Message>,
    output_ended: CancellationToken,
}

impl Drop for AttachOutput {
    fn drop(&mut self) {
        // Cancel before sending, so that an attach client which subscribes
        // after the done message still sees that the output ended.
        self.output_ended.cancel();
        // Sending fails only without attach clients, which then need no done
        // message.
        let _ = self.write_half_tx.send(Message::Done);
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

    /// Create a new attach instance.
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

        if path.exists() {
            debug!(
                "Attach path {} already exist, assuming that we're already listening on it",
                path.display()
            );
            return Ok(());
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
        let (shortened_path, _parent_dir) =
            Listener::<DefaultListener>::default().shorten_socket_path(path)?;
        let addr = UnixAddr::new(&shortened_path).context("create socket addr")?;
        bind(fd.as_raw_fd(), &addr).context("bind socket fd")?;

        let setup = || -> Result<()> {
            let mut permissions = path
                .metadata()
                .context("get attach socket metadata")?
                .permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(path, permissions).context("set attach socket permissions")?;
            listen(&fd, Backlog::MAXCONN).context("listen on socket fd")
        };
        // Nothing listens on the socket if this fails. Remove it, otherwise the
        // next attempt would take it for a listening one.
        if let Err(e) = setup() {
            if let Err(e) = std::fs::remove_file(path) {
                debug!("Unable to remove attach socket {}: {:#}", path.display(), e);
            }
            return Err(e);
        }

        let path = path.to_path_buf();
        task::spawn(
            async move {
                if let Err(e) = Self::start(fd, path, attach, token).await {
                    error!("Attach failure: {:#}", e);
                }
            }
            .instrument(debug_span!("attach")),
        );

        Ok(())
    }

    async fn start(
        fd: OwnedFd,
        path: PathBuf,
        attach: SharedContainerAttach,
        token: CancellationToken,
    ) -> Result<()> {
        debug!("Start listening on attach socket");
        let mut listener =
            UnixSeqpacketListener::try_from(fd).context("create seqpacket listener")?;
        loop {
            // The container exit cancels the token, stop accepting then and
            // remove the socket instead of keeping the listener around for the
            // lifetime of the server.
            let accepted = select! {
                accepted = listener.accept() => accepted,
                _ = token.cancelled() => {
                    debug!("Stopping attach listener because token cancelled");
                    if let Err(e) = std::fs::remove_file(&path) {
                        debug!("Unable to remove attach socket {}: {e}", path.display());
                    }
                    return Ok(());
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
                                error!("Attach read loop failure: {:#}", e);
                            }
                        }
                        .instrument(debug_span!("read_loop")),
                    );

                    let (write_half_rx, output_ended) = attach.subscribe();
                    task::spawn(
                        async move {
                            if let Err(e) =
                                Self::write_loop(write, write_half_rx, output_ended).await
                            {
                                error!("Attach write loop failure: {:#}", e);
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
                _ = token.cancelled() => debug!("Exiting because token cancelled"),
                _ = attach.stdin_closed.cancelled() => debug!("Exiting because stdin got closed"),
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
                _ = stopped() => return Ok(()),
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
                        _ = stopped() => return Ok(()),
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
    #[allow(unsafe_code)]
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

    async fn write_loop(
        socket: Arc<UnixSeqpacket>,
        mut rx: Receiver<Message>,
        output_ended: bool,
    ) -> Result<()> {
        loop {
            // The done message may have been sent before subscribing, so write
            // it once nothing is queued anymore.
            let message = if output_ended {
                rx.try_recv().unwrap_or(Message::Done)
            } else {
                match rx.recv().await {
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
            match message {
                Message::Done => {
                    debug!("Exiting because the container output ended");
                    match socket.send(Self::DONE_PACKET).await {
                        Ok(_) => {
                            debug!("Wrote done packet to client")
                        }
                        Err(ref e) if Self::client_gone(e) => {}
                        Err(e) => bail!("unable to write done packet: {:#}", e),
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
                        match socket.send_vectored(&packet).await {
                            Ok(_) => {
                                debug!("Wrote {} packet {}/{} to client", pipe, idx + 1, len)
                            }
                            Err(ref e) if Self::client_gone(e) => {
                                // The client is gone, so stop receiving output
                                // for it as well.
                                debug!("Exiting because the client closed the connection");
                                return Ok(());
                            }
                            Err(e) => bail!("unable to write packet {}/{}: {:#}", idx + 1, len, e),
                        }
                    }
                }
            }
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
        let mut rx = attach.write_half_tx.subscribe();

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
        let mut rx = attach.write_half_tx.subscribe();
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

    #[tokio::test]
    async fn done_after_output_ended() -> Result<()> {
        let attach = SharedContainerAttach::default();
        drop(attach.output());

        let (server, client) = seqpacket_pair()?;
        let (rx, output_ended) = attach.subscribe();
        assert!(output_ended);
        timeout(
            Duration::from_secs(5),
            Attach::write_loop(server, rx, output_ended),
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
        let mut write_loop = task::spawn(Attach::write_loop(server, rx, false));
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
        let mut attach = SharedContainerAttach::default();
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
        let mut attach = SharedContainerAttach::default();
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
        let mut attach = SharedContainerAttach::default();
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
        let mut attach = SharedContainerAttach::default();
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
        let mut attach = SharedContainerAttach::default();
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
        let mut attach = SharedContainerAttach::default();
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
        let mut attach = SharedContainerAttach::default();
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
