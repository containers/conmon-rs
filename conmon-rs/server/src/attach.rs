use crate::{
    container_io::{Message, Pipe},
    listener::{DefaultListener, Listener},
};
use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use nix::{
    errno::Errno,
    sys::socket::{AddressFamily, Backlog, SockFlag, SockType, UnixAddr, bind, listen, socket},
};
use std::{
    io::{self, IoSlice},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::PermissionsExt,
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncWriteExt, ErrorKind, Interest},
    net::{
        UnixListener,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    select,
    sync::broadcast::{self, Receiver, Sender, error::RecvError},
    task,
};
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
    pub async fn add<T>(
        &mut self,
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

    /// Write a buffer to all attach endpoints.
    pub async fn write(&mut self, m: Message) -> Result<()> {
        if self.write_half_tx.receiver_count() > 0 {
            self.write_half_tx
                .send(m)
                .context("send data message to attach clients")?;
        }
        Ok(())
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
        let listener = UnixListener::from_std(fd.into()).context("create unix listener")?;
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
                Ok((stream, _)) => {
                    debug!("Got new attach stream connection");
                    let (read, write) = stream.into_split();

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
                Err(e) => error!("Unable to accept attach stream: {}", e),
            }
        }
    }

    /// Forward the stdin of a client until it closes, the stdin of the
    /// container got closed or the container exited, which cancels `token`.
    async fn read_loop(
        read_half: OwnedReadHalf,
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

        // `read_buf` grows the buffer back to this capacity once the frozen
        // parts are used up, so no explicit reserve is needed in the loop.
        let mut buf = BytesMut::with_capacity(Self::PACKET_BUF_SIZE);
        loop {
            // In situations we're processing output directly from the I/O streams
            // we need a mechanism to figure out when to stop that doesn't involve reading the
            // number of bytes read.
            // Thus, we need to select on the cancellation token saved in the child.
            // While this could result in a data race, as select statements are racy,
            // we won't interleve these two futures, as one ends execution.
            let read = select! {
                n = Self::read_packet(&read_half, &mut buf) => n,
                _ = stopped() => return Ok(()),
            };

            // A zero byte read on a SOCK_SEQPACKET socket is either an empty
            // packet or the peer closing its write side. Only the latter marks
            // the socket as read closed, so wait for the next readiness and
            // check that flag rather than guessing from another read.
            if let Ok(0) = read {
                let ready = select! {
                    ready = read_half.ready(Interest::READABLE) => {
                        ready.context("wait for attach socket readiness")?
                    }
                    _ = stopped() => return Ok(()),
                };
                if !ready.is_read_closed() {
                    trace!("Ignoring empty packet");
                    continue;
                }
            }

            match read {
                Ok(n) if n > 0 => {
                    // Every read returns a single packet, which is forwarded as
                    // is. Stdin is binary, null bytes are part of the data.
                    // Splitting and freezing shares the buffer without a copy.
                    let data = buf.split().freeze();
                    debug!("Read {} stdin bytes from client", data.len());
                    // Waiting for the container to take the data must not
                    // outlive the container or its stdin.
                    select! {
                        () = attach.send_stdin(data) => {}
                        _ = stopped() => return Ok(()),
                    }
                }
                Err(e) => match Errno::from_raw(e.raw_os_error().context("get OS error")?) {
                    Errno::EIO => {
                        debug!("Stopping read loop because of IO error");
                        return Ok(());
                    }
                    Errno::EBADF => {
                        return Err(Errno::EBADFD.into());
                    }
                    Errno::EAGAIN => {
                        continue;
                    }
                    _ => return Err(e).context("read from attach socket"),
                },
                _ => {
                    // The client closed its write half, no more input will
                    // ever arrive on this connection. The write loop keeps
                    // delivering output independently.
                    debug!("Stopping read loop because there is nothing more to read");
                    if stop_after_stdin_eof {
                        attach.close_container_stdin();
                    }
                    return Ok(());
                }
            }
        }
    }

    /// Read the next packet into `buf`.
    ///
    /// Every read of a SOCK_SEQPACKET socket returns a single packet. tokio's
    /// `read_buf` takes a short read as the socket being drained and waits for
    /// the next readiness event, which leaves packets already queued unread
    /// until another one arrives. `try_read_buf` only clears the readiness
    /// once the socket would block.
    async fn read_packet(read_half: &OwnedReadHalf, buf: &mut BytesMut) -> io::Result<usize> {
        loop {
            read_half.readable().await?;
            match read_half.try_read_buf(buf) {
                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                res => return res,
            }
        }
    }

    async fn write_loop(
        mut write_half: OwnedWriteHalf,
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
                    match write_half.write(Self::DONE_PACKET).await {
                        Ok(_) => {
                            debug!("Wrote done packet to client")
                        }
                        Err(ref e)
                            if e.kind() == ErrorKind::WouldBlock
                                || e.kind() == ErrorKind::BrokenPipe => {}
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
                        match write_half.write_vectored(&packet).await {
                            Ok(_) => {
                                debug!("Wrote {} packet {}/{} to client", pipe, idx + 1, len)
                            }
                            Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                            Err(ref e) if e.kind() == ErrorKind::BrokenPipe => {
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
    use crate::{container_io::ContainerIO, container_log::ContainerLog};
    use nix::sys::socket::{MsgFlags, Shutdown, connect, send, shutdown, socketpair};
    use std::{
        io,
        pin::Pin,
        task::{Context as TaskContext, Poll},
    };
    use tokio::{
        io::{AsyncRead, AsyncReadExt, ReadBuf},
        net::UnixStream,
        time::{Duration, timeout},
    };

    /// A connected pair of packet sockets, like the attach clients use.
    fn seqpacket_pair() -> Result<(UnixStream, UnixStream)> {
        let (a, b) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        )?;
        Ok((
            UnixStream::from_std(a.into())?,
            UnixStream::from_std(b.into())?,
        ))
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
        let mut attach = SharedContainerAttach::default();
        let mut rx = attach.write_half_tx.subscribe();

        let stdout = attach.output();
        let stderr = stdout.clone();

        drop(stderr);
        attach
            .write(Message::Data(Bytes::from_static(b"last"), Pipe::StdOut))
            .await?;
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
            Some(message_tx.clone()),
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
            Some(message_tx),
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

        let (server, mut client) = seqpacket_pair()?;
        let (_, write) = server.into_split();
        let (rx, output_ended) = attach.subscribe();
        assert!(output_ended);
        timeout(
            Duration::from_secs(5),
            Attach::write_loop(write, rx, output_ended),
        )
        .await??;

        let mut buf = [1];
        client.read_exact(&mut buf).await?;
        assert_eq!(&buf, Attach::DONE_PACKET);
        Ok(())
    }

    #[tokio::test]
    async fn write_loop_stops_on_broken_pipe() -> Result<()> {
        let mut attach = SharedContainerAttach::default();
        let (server, client) = seqpacket_pair()?;
        let (_, write) = server.into_split();
        drop(client);

        let (rx, _) = attach.subscribe();
        attach
            .write(Message::Data(Bytes::from_static(b"data"), Pipe::StdOut))
            .await?;
        timeout(Duration::from_secs(5), Attach::write_loop(write, rx, false)).await??;
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
        let (client, server) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        )?;
        let (read_half, _write_half) = UnixStream::from_std(server.into())?.into_split();

        let mut attach = SharedContainerAttach::default();
        let token = CancellationToken::new();
        let task = task::spawn(Attach::read_loop(read_half, attach.clone(), token.clone()));

        for packet in [&b"a\0b"[..], b"\0", b"c"] {
            send(client.as_raw_fd(), packet, MsgFlags::empty())?;
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
    async fn stdin_eof_keeps_listener() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("attach");
        let token = CancellationToken::new();
        let mut attach = SharedContainerAttach::default();
        attach.add(&path, token.clone(), true).await?;

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
        // container, but the container keeps running.
        let first = connect_client()?;
        shutdown(first.as_raw_fd(), Shutdown::Write)?;
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
        let (server, mut client) = seqpacket_pair()?;
        let (read, _write) = server.into_split();
        let read_loop = task::spawn(Attach::read_loop(read, attach.clone(), token.clone()));

        // Nothing reads the stdin, so the read loop ends up waiting to send.
        for i in 0..=STDIN_CHANNEL_SIZE {
            client.write_all(&[u8::try_from(i)? + 1]).await?;
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
            let (read, _write) = server.into_split();
            drop(client);

            timeout(
                Duration::from_secs(5),
                Attach::read_loop(read, attach.clone(), CancellationToken::new()),
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
    async fn stdin_without_dropping() -> Result<()> {
        let mut attach = SharedContainerAttach::default();
        let (server, mut client) = seqpacket_pair()?;
        let (read, _write) = server.into_split();
        let read_loop = task::spawn(Attach::read_loop(
            read,
            attach.clone(),
            CancellationToken::new(),
        ));

        // More packets than the channel capacity, which have to wait for the
        // reader instead of getting dropped.
        const PACKETS: u8 = 64;
        for i in 0..PACKETS {
            client.write_all(&[i + 1]).await?;
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
