use anyhow::{bail, Context, Result};
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
    io::{AsyncBufReadExt, AsyncWriteExt, BufStream},
    net::{UnixListener, UnixStream},
    select,
    sync::broadcast::{self, Receiver, Sender},
    task,
};

#[derive(Clone, Debug)]
/// Attach handles the attach socket IO of a container.
pub struct Attach;

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
        // TODO: The receivers have to be passed to the container_io module and handled there.
        let (stdin_tx, _stdin_rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (stdout_tx, _stdout_rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (stderr_tx, _stderr_rx) = broadcast::channel(BROADCAST_CAPACITY);
        task::spawn(async move { Self::start_listening(fd, stdin_tx, stdout_tx, stderr_tx).await });

        Ok(Self {})
    }

    async fn start_listening(
        fd: RawFd,
        stdin: Sender<String>,
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
                }
                Err(e) => error!("Unable to accept attach stream: {}", e),
            }
        }
    }

    async fn handle_client(
        stream: UnixStream,
        stdin: Sender<String>,
        mut stdout: Receiver<Vec<u8>>,
        mut stderr: Receiver<Vec<u8>>,
    ) -> Result<()> {
        debug!("Got new attach stream connection");

        enum Pipe {
            Stdout = 2,
            Stderr = 3,
        }

        let mut io = BufStream::new(stream);
        loop {
            let mut line = String::new();

            select! {
                res = stdout.recv() => {
                    let mut data = res?;
                    debug!("Got {} byte stdout data", data.len());
                    data.insert(0, Pipe::Stdout as u8);
                    io.write_all(&data).await.context("write stdout")?;
                },
                res = stderr.recv() => {
                    let mut data = res?;
                    debug!("Got {} byte stderr data", data.len());
                    data.insert(0, Pipe::Stderr as u8);
                    io.write_all(&data).await.context("write stderr")?;
                },
                res = io.read_line(&mut line) => {
                    let len = res?;
                    if len > 0 {
                        debug!("Got stdin line of len {}", len);
                        stdin.send(line).context("write stdin")?;
                    }
                },
            };
        }
    }
}
