use crate::{
    container_io::{Message, Pipe},
    listener::{DefaultListener, Listener},
};
use anyhow::{Context, Result, bail};
use nix::{
    errno::Errno,
    sys::socket::{AddressFamily, Backlog, SockFlag, SockType, UnixAddr, bind, listen, socket},
};
use std::{
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::PermissionsExt,
    },
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ErrorKind},
    net::{
        UnixListener,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    select,
    sync::broadcast::{self, Receiver, Sender},
    task,
    time::{self, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error};

#[derive(Debug)]
/// A shared container attach abstraction.
pub struct SharedContainerAttach {
    read_half_rx: Receiver<Vec<u8>>,
    read_half_tx: Sender<Vec<u8>>,
    write_half_tx: Sender<Message>,
}

impl Default for SharedContainerAttach {
    fn default() -> Self {
        let (read_half_tx, read_half_rx) = broadcast::channel(2);
        let (write_half_tx, _) = broadcast::channel(2);
        Self {
            read_half_rx,
            read_half_tx,
            write_half_tx,
        }
    }
}

impl Clone for SharedContainerAttach {
    fn clone(&self) -> Self {
        Self {
            read_half_rx: self.read_half_tx.subscribe(),
            read_half_tx: self.read_half_tx.clone(),
            write_half_tx: self.write_half_tx.clone(),
        }
    }
}

impl SharedContainerAttach {
    /// Add a new attach endpoint to this shared container attach instance.
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
            stop_after_stdin_eof,
        )
        .context("create attach endpoint")
    }

    /// Read from all attach endpoints standard input and return the first result.
    pub async fn read(&mut self) -> Result<Vec<u8>> {
        self.read_half_rx
            .recv()
            .await
            .context("receive attach message")
    }

    /// Try to read from all attach endpoints standard input and return the first result.
    pub fn try_read(&mut self) -> Result<Vec<u8>> {
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

    /// Retrieve the stdin sender.
    pub fn stdin(&self) -> &Sender<Vec<u8>> {
        &self.read_half_tx
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
        read_half_tx: Sender<Vec<u8>>,
        write_half_tx: Sender<Message>,
        token: CancellationToken,
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

        let metadata = path.metadata()?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);

        listen(&fd, Backlog::MAXCONN).context("listen on socket fd")?;

        task::spawn(
            async move {
                if let Err(e) =
                    Self::start(fd, read_half_tx, write_half_tx, token, stop_after_stdin_eof).await
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
        read_half_tx: Sender<Vec<u8>>,
        write_half_tx: Sender<Message>,
        token: CancellationToken,
        stop_after_stdin_eof: bool,
    ) -> Result<()> {
        debug!("Start listening on attach socket");
        let listener = UnixListener::from_std(fd.into()).context("create unix listener")?;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    debug!("Got new attach stream connection");
                    let (read, write) = stream.into_split();

                    let read_half_tx_clone = read_half_tx.clone();
                    let token_clone = token.clone();
                    task::spawn(
                        async move {
                            if let Err(e) = Self::read_loop(
                                read,
                                read_half_tx_clone,
                                token_clone,
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

    async fn read_loop(
        mut read_half: OwnedReadHalf,
        tx: Sender<Vec<u8>>,
        token: CancellationToken,
        stop_after_stdin_eof: bool,
    ) -> Result<()> {
        let mut buf = vec![0; Self::PACKET_BUF_SIZE];
        loop {
            // In situations we're processing output directly from the I/O streams
            // we need a mechanism to figure out when to stop that doesn't involve reading the
            // number of bytes read.
            // Thus, we need to select on the cancellation token saved in the child.
            // While this could result in a data race, as select statements are racy,
            // we won't interleve these two futures, as one ends execution.
            select! {
                n = read_half.read(&mut buf) => {
                    match n {
                        Ok(n) if n > 0 => {
                            let end = buf[..n].iter().position(|&x| x == 0).unwrap_or(n);
                            let data = buf[..end].to_vec();
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
                            _ => error!(
                                "Unable to read from file descriptor: {} {}",
                                e,
                                e.raw_os_error().context("get OS error")?
                            ),
                        },
                        _ if stop_after_stdin_eof => {
                            debug!("Stopping read loop because there is nothing more to read");
                            token.cancel();
                            return Ok(());
                        }
                        _ => time::sleep(Duration::from_millis(500)).await, // avoid busy looping
                    }
                }
                _ = token.cancelled() => {
                    debug!("Exiting because token cancelled");
                    return Ok(());
                }
            }
        }
    }

    async fn write_loop(mut write_half: OwnedWriteHalf, mut rx: Receiver<Message>) -> Result<()> {
        loop {
            match rx.recv().await.context("receive message")? {
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
                    let packets = buf
                        .chunks(Self::PACKET_BUF_SIZE - 1)
                        .map(|x| {
                            let mut y = Vec::with_capacity(1 + x.len());
                            y.push(p);
                            y.extend_from_slice(x);
                            y
                        })
                        .collect::<Vec<_>>();

                    let len = packets.len();
                    for (idx, packet) in packets.iter().enumerate() {
                        match write_half.write(packet).await {
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
