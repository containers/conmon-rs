use crate::{container_io::Pipe, listener};
use anyhow::{bail, Context, Result};
use nix::sys::socket::{bind, listen, socket, AddressFamily, SockFlag, SockType, UnixAddr};
use std::{
    os::unix::{
        fs::PermissionsExt,
        io::{FromRawFd, RawFd},
        net,
    },
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{ErrorKind, Interest, Ready},
    net::{UnixListener, UnixStream},
    sync::RwLock,
    task,
    time::{timeout, Duration},
};
use tracing::{debug, debug_span, error, Instrument};

#[derive(Debug, Clone, Default)]
/// A shared container attach abstraction.
pub struct SharedContainerAttach(Arc<RwLock<Vec<Attach>>>);

impl SharedContainerAttach {
    /// Add a new attach endpoint to this shared container attach instance.
    pub async fn add(&self, attach: Attach) {
        self.0.write().await.push(attach);
    }

    /// Try to read from all attach endpoints standard input and return the first result.
    pub async fn try_read(&self) -> Result<Option<Vec<u8>>> {
        self.cleanup().await;
        for attach in self.0.read().await.iter() {
            if let Some(data) = attach.try_read().await? {
                return Ok(data.into());
            }
        }
        Ok(None)
    }

    /// Write a buffer to all attach endpoints.
    pub async fn write<T>(&self, pipe: Pipe, buf: T) -> Result<()>
    where
        T: AsRef<[u8]>,
    {
        self.cleanup().await;
        for attach in self.0.read().await.iter() {
            attach
                .write(pipe, &buf)
                .await
                .context("write to attach endpoint")?;
        }
        Ok(())
    }

    /// Remove attach endpoints which do not exist any more.
    async fn cleanup(&self) {
        self.0.write().await.retain(|x| {
            let exists = x.path.exists();
            if !exists {
                debug!("Cleanup attach endpoint: {}", x.path.display())
            }
            exists
        });
    }
}

/// The size of an attach packet.
const ATTACH_PACKET_BUF_SIZE: usize = 8192;

type Clients = Arc<RwLock<Vec<UnixStream>>>;

#[derive(Clone, Debug)]
/// Attach handles the attach socket IO of a container.
pub struct Attach {
    clients: Clients,
    path: PathBuf,
}

impl Attach {
    /// Create a new attach instance.
    pub fn new(socket_path: &Path) -> Result<Self> {
        debug!("Creating attach socket: {}", socket_path.display());

        if socket_path.exists() {
            bail!(
                "Attach socket path already exists: {}",
                socket_path.display()
            )
        }

        let fd = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
            None,
        )
        .context("bind socket")?;

        // keep parent_fd in scope until the bind, or else the socket will not work
        let (shortened_path, _parent_dir) = listener::shorten_socket_path(socket_path)?;
        let addr = UnixAddr::new(&shortened_path).context("create socket addr")?;
        bind(fd, &addr).context("bind socket fd")?;

        let metadata = socket_path.metadata()?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);

        listen(fd, 10).context("listen on socket fd")?;

        let clients = Arc::new(RwLock::new(vec![]));
        let clients_clone = clients.clone();
        task::spawn(
            async move {
                if let Err(e) = Self::start_listening(fd, clients_clone).await {
                    error!("Attach failure: {:#}", e);
                }
            }
            .instrument(debug_span!("attach")),
        );

        Ok(Self {
            clients,
            path: socket_path.into(),
        })
    }

    async fn start_listening(fd: RawFd, clients: Clients) -> Result<()> {
        debug!("Start listening on attach socket");
        let listener = UnixListener::from_std(unsafe { net::UnixListener::from_raw_fd(fd) })?;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    debug!("Got new attach stream connection");
                    clients.write().await.push(stream);
                }
                Err(e) => error!("Unable to accept attach stream: {}", e),
            }
        }
    }

    /// Try to read from all streams standard input and return the first result.
    pub async fn try_read(&self) -> Result<Option<Vec<u8>>> {
        for stream in self.clients.read().await.iter() {
            let ready = if let Some(ready) =
                Self::default_readiness_timeout(Interest::READABLE, stream).await?
            {
                ready
            } else {
                continue;
            };

            if ready.is_readable() {
                let mut buf = vec![0; ATTACH_PACKET_BUF_SIZE];
                match stream.try_read(&mut buf) {
                    Ok(n) if n > 0 => {
                        if let Some(first_zero_idx) = buf.iter().position(|&x| x == 0) {
                            buf.resize(first_zero_idx, 0);
                        }
                        debug!("Read {} stdin bytes from client", buf.len());
                        return Ok(buf.into());
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Err(e) => {
                        return Err(e.into());
                    }
                    _ => {}
                }
            }
        }

        Ok(None)
    }

    /// Write a buffer to all attached clients.
    pub async fn write<T>(&self, pipe: Pipe, buf: T) -> Result<()>
    where
        T: AsRef<[u8]>,
    {
        let packets = buf
            .as_ref()
            .chunks(ATTACH_PACKET_BUF_SIZE - 1)
            .map(|x| {
                let mut y = x.to_vec();
                let p = match pipe {
                    Pipe::StdOut => 2,
                    Pipe::StdErr => 3,
                };
                y.insert(0, p);
                y.resize(ATTACH_PACKET_BUF_SIZE, 0);
                y
            })
            .collect::<Vec<_>>();

        let mut cleanup_idxs = vec![];
        let mut clients = self.clients.write().await;

        for (idx, stream) in clients.iter().enumerate() {
            let ready = if let Some(ready) =
                Self::default_readiness_timeout(Interest::WRITABLE, stream).await?
            {
                ready
            } else {
                continue;
            };

            if ready.is_write_closed() {
                cleanup_idxs.push(idx);
                continue;
            }

            if ready.is_writable() {
                for packet in &packets {
                    match stream.try_write(packet) {
                        Ok(_) => {
                            debug!("Wrote {} packet to client", &pipe.as_ref());
                        }
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                        Err(e) => {
                            Self::cleanup_clients(&mut clients, &cleanup_idxs).await;
                            return Err(e.into());
                        }
                    }
                }
            }
        }

        Self::cleanup_clients(&mut clients, &cleanup_idxs).await;
        Ok(())
    }

    async fn default_readiness_timeout(
        interest: Interest,
        stream: &UnixStream,
    ) -> Result<Option<Ready>> {
        match timeout(Duration::from_millis(100), stream.ready(interest)).await {
            Ok(r) => Ok(Some(r.context("wait for stream to become ready")?)),
            _ => Ok(None),
        }
    }

    async fn cleanup_clients(clients: &mut Vec<UnixStream>, idxs: &[usize]) {
        for i in idxs.iter().rev() {
            debug!("Cleanup stale attach client with index: {}", i);
            clients.remove(*i);
        }
    }
}
