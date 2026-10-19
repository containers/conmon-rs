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

/// Capacity of the stdin broadcast channel towards the container.
const STDIN_CHANNEL_SIZE: usize = 16;

/// Capacity of the output broadcast channel towards the attach clients. A
/// client which falls further behind than this loses output, so it matches
/// what the stdio channels buffer for exec.
const OUTPUT_CHANNEL_SIZE: usize = 100;

#[derive(Debug)]
/// A shared container attach abstraction.
pub struct SharedContainerAttach {
    read_half_rx: Receiver<Bytes>,
    read_half_tx: Sender<Bytes>,
    write_half_tx: Sender<Message>,

    /// Cancelled once a client with `stop_after_stdin_eof` closed its stdin,
    /// which closes the stdin of the container for good. It is separate from
    /// the token of the container, which only the exit may cancel.
    stdin_closed: CancellationToken,
}

impl Default for SharedContainerAttach {
    fn default() -> Self {
        let (read_half_tx, read_half_rx) = broadcast::channel(STDIN_CHANNEL_SIZE);
        let (write_half_tx, _) = broadcast::channel(OUTPUT_CHANNEL_SIZE);
        Self {
            read_half_rx,
            read_half_tx,
            write_half_tx,
            stdin_closed: CancellationToken::new(),
        }
    }
}

impl Clone for SharedContainerAttach {
    fn clone(&self) -> Self {
        Self {
            read_half_rx: self.read_half_tx.subscribe(),
            read_half_tx: self.read_half_tx.clone(),
            write_half_tx: self.write_half_tx.clone(),
            stdin_closed: self.stdin_closed.clone(),
        }
    }
}

impl SharedContainerAttach {
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
        Attach::create(
            socket_path,
            self.read_half_tx.clone(),
            self.write_half_tx.clone(),
            token,
            self.stdin_closed.clone(),
            stop_after_stdin_eof,
        )
        .context("create attach endpoint")
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

    /// Write a buffer to all attach endpoints.
    pub async fn write(&mut self, m: Message) -> Result<()> {
        if self.write_half_tx.receiver_count() > 0 {
            self.write_half_tx
                .send(m)
                .context("send data message to attach clients")?;
        }
        Ok(())
    }

    /// Check if there are any active attach readers.
    pub fn has_readers(&self) -> bool {
        self.write_half_tx.receiver_count() > 0
    }

    /// Subscribe to the container output sent to the attach endpoints.
    pub fn subscribe(&self) -> Receiver<Message> {
        self.write_half_tx.subscribe()
    }

    /// Retrieve the stdin sender.
    pub fn stdin(&self) -> &Sender<Bytes> {
        &self.read_half_tx
    }

    /// The token cancelled once the stdin of the container got closed.
    pub fn stdin_closed(&self) -> &CancellationToken {
        &self.stdin_closed
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
        read_half_tx: Sender<Bytes>,
        write_half_tx: Sender<Message>,
        token: CancellationToken,
        stdin_closed: CancellationToken,
        stop_after_stdin_eof: bool,
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
                if let Err(e) = Self::start(
                    fd,
                    path,
                    read_half_tx,
                    write_half_tx,
                    token,
                    stdin_closed,
                    stop_after_stdin_eof,
                )
                .await
                {
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
        read_half_tx: Sender<Bytes>,
        write_half_tx: Sender<Message>,
        token: CancellationToken,
        stdin_closed: CancellationToken,
        stop_after_stdin_eof: bool,
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

                    let read_half_tx_clone = read_half_tx.clone();
                    let token_clone = token.clone();
                    let stdin_closed_clone = stdin_closed.clone();
                    task::spawn(
                        async move {
                            if let Err(e) = Self::read_loop(
                                read,
                                read_half_tx_clone,
                                token_clone,
                                stdin_closed_clone,
                                stop_after_stdin_eof,
                            )
                            .await
                            {
                                error!("Attach read loop failure: {:#}", e);
                            }
                        }
                        .instrument(debug_span!("read_loop")),
                    );

                    let write_half_rx = write_half_tx.subscribe();
                    task::spawn(
                        async move {
                            if let Err(e) = Self::write_loop(write, write_half_rx).await {
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
        tx: Sender<Bytes>,
        token: CancellationToken,
        stdin_closed: CancellationToken,
        stop_after_stdin_eof: bool,
    ) -> Result<()> {
        let stopped = async || {
            select! {
                _ = token.cancelled() => debug!("Exiting because token cancelled"),
                _ = stdin_closed.cancelled() => debug!("Exiting because stdin got closed"),
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
                    tx.send(data).context("send data message")?;
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
                        stdin_closed.cancel();
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

    async fn write_loop(mut write_half: OwnedWriteHalf, mut rx: Receiver<Message>) -> Result<()> {
        loop {
            let message = match rx.recv().await {
                Ok(message) => message,
                Err(RecvError::Lagged(n)) => {
                    warn!("Attach client too slow, skipped {n} messages");
                    continue;
                }
                Err(RecvError::Closed) => {
                    debug!("Exiting because container I/O is gone");
                    return Ok(());
                }
            };
            match message {
                Message::Done => {
                    debug!("Exiting because token cancelled");
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
                            Err(ref e) if e.kind() == ErrorKind::BrokenPipe => break,
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
    use nix::sys::socket::{MsgFlags, Shutdown, connect, send, shutdown, socketpair};
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::time::Duration;
    use tokio::{net::UnixStream, time::timeout};

    #[tokio::test]
    async fn read_loop_forwards_packets_with_null_bytes() -> Result<()> {
        let (client, server) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        )?;
        let (read_half, _write_half) =
            UnixStream::from_std(StdUnixStream::from(server))?.into_split();

        let (tx, mut rx) = broadcast::channel(STDIN_CHANNEL_SIZE);
        let token = CancellationToken::new();
        let task = task::spawn(Attach::read_loop(
            read_half,
            tx,
            token.clone(),
            CancellationToken::new(),
            false,
        ));

        for packet in [&b"a\0b"[..], b"\0", b"c"] {
            send(client.as_raw_fd(), packet, MsgFlags::empty())?;
        }

        for expected in [&b"a\0b"[..], b"\0", b"c"] {
            let data = timeout(Duration::from_secs(5), rx.recv()).await??;
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
}
