use crate::{container_io::Pipe, listener};
use anyhow::{bail, Context, Result};
use nix::{
    errno::Errno,
    sys::socket::{bind, listen, socket, AddressFamily, SockFlag, SockType, UnixAddr},
};
use std::{
    convert::From,
    os::unix::{
        fs::PermissionsExt,
        io::{FromRawFd, RawFd},
        net,
    },
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ErrorKind},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixListener,
    },
    sync::broadcast::{self, Receiver, Sender},
    task,
};
use tracing::{debug, debug_span, error, Instrument};

#[derive(Debug)]
/// A shared container attach abstraction.
pub struct SharedContainerAttach {
    read_half_rx: Receiver<Vec<u8>>,
    read_half_tx: Sender<Vec<u8>>,
    write_half_tx: Sender<(Pipe, Vec<u8>)>,
}

impl Default for SharedContainerAttach {
    fn default() -> Self {
        let (read_half_tx, read_half_rx) = broadcast::channel(1000);
        let (write_half_tx, _) = broadcast::channel(1000);
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
    pub async fn add<T>(&mut self, socket_path: T) -> Result<()>
    where
        T: AsRef<Path>,
        PathBuf: From<T>,
    {
        Attach::create(
            socket_path,
            self.read_half_tx.clone(),
            self.write_half_tx.clone(),
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

    /// Write a buffer to all attach endpoints.
    pub async fn write<T>(&mut self, pipe: Pipe, buf: T) -> Result<()>
    where
        T: AsRef<[u8]>,
    {
        if self.write_half_tx.receiver_count() > 0 {
            self.write_half_tx
                .send((pipe, buf.as_ref().into()))
                .context("send data message to attach clients")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
/// Attach handles the attach socket IO of a container.
struct Attach;

impl Attach {
    /// The size of an attach packet.
    const PACKET_BUF_SIZE: usize = 8192;

    /// The packet indicating that we're done writing.
    const DONE_PACKET: &'static [u8; Self::PACKET_BUF_SIZE] = &[0; Self::PACKET_BUF_SIZE];

    /// Create a new attach instance.
    fn create<T>(
        socket_path: T,
        read_half_tx: Sender<Vec<u8>>,
        write_half_tx: Sender<(Pipe, Vec<u8>)>,
    ) -> Result<()>
    where
        T: AsRef<Path>,
        PathBuf: From<T>,
    {
        let path = socket_path.as_ref();
        debug!("Creating attach socket: {}", path.display());

        if path.exists() {
            bail!("Attach socket path already exists: {}", path.display())
        }

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
        bind(fd, &addr).context("bind socket fd")?;

        let metadata = path.metadata()?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);

        listen(fd, 10).context("listen on socket fd")?;

        task::spawn(
            async move {
                if let Err(e) = Self::start(fd, read_half_tx, write_half_tx).await {
                    error!("Attach failure: {:#}", e);
                }
            }
            .instrument(debug_span!("attach")),
        );

        Ok(())
    }

    async fn start(
        fd: RawFd,
        read_half_tx: Sender<Vec<u8>>,
        write_half_tx: Sender<(Pipe, Vec<u8>)>,
    ) -> Result<()> {
        debug!("Start listening on attach socket");
        let listener = UnixListener::from_std(unsafe { net::UnixListener::from_raw_fd(fd) })?;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    debug!("Got new attach stream connection");
                    let (read, write) = stream.into_split();

                    let read_half_tx_clone = read_half_tx.clone();
                    task::spawn(
                        async move {
                            if let Err(e) = Self::read_loop(read, read_half_tx_clone).await {
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

    async fn read_loop(mut read_half: OwnedReadHalf, tx: Sender<Vec<u8>>) -> Result<()> {
        loop {
            let mut buf = vec![0; Self::PACKET_BUF_SIZE];
            match read_half.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    if let Some(first_zero_idx) = buf.iter().position(|&x| x == 0) {
                        buf.resize(first_zero_idx, 0);
                    }
                    debug!("Read {} stdin bytes from client", buf.len());
                    tx.send(buf).context("send data message")?;
                }
                Ok(n) if n == 0 => {
                    debug!("Stopping read loop because no more data to read");
                    return Ok(());
                }
                Err(e) => match Errno::from_i32(e.raw_os_error().context("get OS error")?) {
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
                _ => {}
            }
        }
    }

    async fn write_loop(
        mut write_half: OwnedWriteHalf,
        mut rx: Receiver<(Pipe, Vec<u8>)>,
    ) -> Result<()> {
        loop {
            let (pipe, buf) = rx.recv().await?;

            let mut packets = buf
                .chunks(Self::PACKET_BUF_SIZE - 1)
                .map(|x| {
                    let mut y = x.to_vec();
                    let p = match pipe {
                        Pipe::StdOut => 2,
                        Pipe::StdErr => 3,
                    };
                    y.insert(0, p);
                    y.resize(Self::PACKET_BUF_SIZE, 0);
                    y
                })
                .collect::<Vec<_>>();
            packets.push(Self::DONE_PACKET.to_vec());

            let len = packets.len() - 1;
            for (idx, packet) in packets.iter().enumerate() {
                match write_half.write(packet).await {
                    Ok(_) => {
                        debug!("Wrote {} packet {}/{} to client", pipe, idx, len)
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Err(ref e) if e.kind() == ErrorKind::BrokenPipe => break,
                    Err(e) => bail!("unable to write packet {}/{}: {:#}", idx, len, e),
                }
            }
        }
    }
}
