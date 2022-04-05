use anyhow::{Context, Result};
use log::{debug, error};
use nix::sys::socket::{
    bind, listen, socket, AddressFamily, SockAddr, SockFlag, SockType, UnixAddr,
};
use std::{
    fs::remove_file,
    io::{BufReader, BufWriter, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        io::{FromRawFd, RawFd},
        net::{UnixListener, UnixStream},
    },
    path::Path,
    thread,
};

#[derive(Clone, Debug)]
/// Attach handles the attach socket IO of a container.
pub struct Attach;

impl Attach {
    /// Create a a new attach instance.
    pub fn new(socket_path: &Path) -> Result<Self> {
        debug!("Creating attach socket: {}", socket_path.display());

        if socket_path.exists() {
            remove_file(&socket_path).context("remove existing socket file")?;
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
        thread::spawn(move || Self::start_listening(fd));

        Ok(Self {})
    }

    fn start_listening(fd: RawFd) -> Result<()> {
        debug!("Start listening on attach socket");
        let listener = unsafe { UnixListener::from_raw_fd(fd) };
        for stream in listener.incoming() {
            thread::spawn(|| Self::handle_client(stream?));
        }
        Ok(())
    }

    fn handle_client(stream: UnixStream) -> Result<()> {
        debug!("Got new attach stream connection");

        let mut reader = BufReader::new(&stream);
        let mut writer = BufWriter::new(&stream);

        const BUF_SIZE: usize = 8192;
        let mut buf = vec![0; BUF_SIZE];

        // TODO: implement me
        buf[0] = 2;
        writer.write_all(&buf)?;
        buf[0] = 3;
        writer.write_all(&buf)?;

        loop {
            match reader.read(&mut buf) {
                Ok(n) if n > 0 => {
                    debug!("Read {} bytes", n);
                }
                Err(e) => error!("Unable to read from file descriptor: {}", e),
                _ => {}
            }
        }
    }
}
