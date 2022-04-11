use anyhow::{bail, Context, Result};
use getset::Getters;
use log::{debug, error};
use nix::sys::socket::{
    bind, listen, socket, AddressFamily, SockAddr, SockFlag, SockType, UnixAddr,
};
use std::{
    os::unix::{
        fs::PermissionsExt,
        io::{FromRawFd, RawFd},
        net,
    },
    path::Path,
};
use tokio::{
    io::{ErrorKind, Interest},
    net::{UnixListener, UnixStream},
    select,
    sync::broadcast::{self, Receiver, Sender},
    task,
};

/// The size of an attach packet.
pub const ATTACH_PACKET_BUF_SIZE: usize = 8192;

// TODO: remove this when being used
#[allow(dead_code)]
#[derive(Clone, Debug, Getters)]
/// Attach handles the attach socket IO of a container.
pub struct Attach {
    #[getset(get = "pub")]
    stdin: Sender<Vec<u8>>,

    #[getset(get = "pub")]
    stdout: Sender<Vec<u8>>,

    #[getset(get = "pub")]
    stderr: Sender<Vec<u8>>,
}

impl Attach {
    /// Create a a new attach instance.
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

        let addr = UnixAddr::new(socket_path).context("create socket addr")?;
        bind(fd, &SockAddr::Unix(addr)).context("bind socket fd")?;

        let metadata = socket_path.metadata()?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);

        listen(fd, 10).context("listen on socket fd")?;

        const BROADCAST_CAPACITY: usize = 64;

        let (stdin_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (stdout_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (stderr_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        let (stdin_clone, stdout_clone, stderr_clone) =
            (stdin_tx.clone(), stdout_tx.clone(), stderr_tx.clone());

        task::spawn(async move {
            Self::start_listening(fd, stdin_clone, stdout_clone, stderr_clone).await
        });

        Ok(Self {
            stdin: stdin_tx,
            stdout: stdout_tx,
            stderr: stderr_tx,
        })
    }

    async fn start_listening(
        fd: RawFd,
        stdin: Sender<Vec<u8>>,
        stdout: Sender<Vec<u8>>,
        stderr: Sender<Vec<u8>>,
    ) -> Result<()> {
        debug!("Start listening on attach socket");
        let listener = UnixListener::from_std(unsafe { net::UnixListener::from_raw_fd(fd) })?;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let stdin_tx = stdin.clone();
                    let stdout_rx = stdout.subscribe();
                    let stderr_rx = stderr.subscribe();
                    task::spawn(async move {
                        if let Err(e) =
                            Self::handle_client(stream, stdin_tx, stdout_rx, stderr_rx).await
                        {
                            error!("Unable to handle attach client: {:#}", e)
                        }
                    });

                    // TODO: remove me when integrated
                    let stdout_clone = stdout.clone();
                    let stderr_clone = stderr.clone();
                    task::spawn(async move {
                        stdout_clone.send("Hello stdout!".into()).unwrap();
                        stderr_clone.send("Hello stderr!".into()).unwrap();
                    });
                }
                Err(e) => error!("Unable to accept attach stream: {}", e),
            }
        }
    }

    async fn handle_client(
        stream: UnixStream,
        _stdin: Sender<Vec<u8>>,
        mut stdout: Receiver<Vec<u8>>,
        mut stderr: Receiver<Vec<u8>>,
    ) -> Result<()> {
        debug!("Got new attach stream connection");

        // The first byte indicates that we either handle stdout or stderr.
        // This behavior is inherited from the original conmon.
        enum Pipe {
            Stdout = 2,
            Stderr = 3,
        }

        loop {
            let ready = stream
                .ready(Interest::READABLE | Interest::WRITABLE)
                .await?;

            if ready.is_readable() {
                let mut buf = vec![0; ATTACH_PACKET_BUF_SIZE];
                match stream.try_read(&mut buf) {
                    Ok(n) if n > 0 => {
                        if let Some(first_zero_idx) = buf.iter().position(|&x| x == 0) {
                            buf.resize(first_zero_idx, 0);
                        }
                        debug!("Read {} stdin bytes", buf.len());
                        // TODO: send stdin when integrated
                        // stdin.send(buf)?;
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(e.into()),
                    _ => {}
                }
            }

            if ready.is_writable() {
                let packets = select! {
                    res = stdout.recv() => {
                        res?.chunks(ATTACH_PACKET_BUF_SIZE - 1)
                            .map(|x| {
                                let mut y = x.to_vec();
                                y.insert(0, Pipe::Stdout as u8);
                                y.resize(ATTACH_PACKET_BUF_SIZE, 0);
                                y
                            })
                            .collect::<Vec<_>>()
                    },
                    res = stderr.recv() => {
                        res?.chunks(ATTACH_PACKET_BUF_SIZE - 1)
                            .map(|x| {
                                let mut y = x.to_vec();
                                y.insert(0, Pipe::Stderr as u8);
                                y.resize(ATTACH_PACKET_BUF_SIZE, 0);
                                y
                            })
                            .collect::<Vec<_>>()
                    },
                };

                for packet in packets {
                    match stream.try_write(&packet) {
                        Ok(_) => {
                            debug!("Wrote stdout/stderr packet");
                        }
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
    }
}
