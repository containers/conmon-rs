//! Terminal console functionalities.

use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, Message, Pipe, STDIO_CHANNEL_SIZE, StdioSender, TempFile},
    container_log::SharedContainerLog,
    listener::{DefaultListener, Listener},
};
use anyhow::{Context as _, Result};
use libc::{TIOCSWINSZ, winsize};
use nix::{
    cmsg_space,
    fcntl::{self, FcntlArg, OFlag},
    sys::{
        socket::{self, ControlMessageOwned, MsgFlags},
        termios::{self, OutputFlags, SetArg, SpecialCharacterIndices},
    },
};
use std::{
    io::{self, IoSliceMut, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::{fs::PermissionsExt, io::RawFd},
    },
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, ready},
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, Interest, ReadBuf, unix::AsyncFd},
    net::{UnixListener, UnixStream},
    select,
    sync::mpsc::{self, Receiver, Sender},
    task,
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error};

#[derive(Debug)]
pub struct Terminal {
    /// The socket the runtime connects to, removed with the terminal.
    path: TempFile,
    connected_rx: Receiver<OwnedFd>,

    /// The output channel. It exists before the runtime connects, so that
    /// the output of an exec can be consumed while its runtime runs. The
    /// sender moves into the read loop on the connection.
    message_rx: Option<async_channel::Receiver<Message>>,
    message_tx: Option<StdioSender>,
    tty: Option<Weak<TerminalFd>>,
    logger: SharedContainerLog,
    attach: SharedContainerAttach,

    /// Whether a consumer reads the message channel, see [`StdioSender`].
    consumed: Arc<AtomicBool>,
}

impl Terminal {
    pub fn path(&self) -> &Path {
        self.path.path()
    }
    pub fn message_rx(&self) -> Option<&async_channel::Receiver<Message>> {
        self.message_rx.as_ref()
    }
    pub fn message_rx_mut(&mut self) -> &mut Option<async_channel::Receiver<Message>> {
        &mut self.message_rx
    }
    fn tty(&self) -> &Option<Weak<TerminalFd>> {
        &self.tty
    }
    fn set_tty(&mut self, val: Option<Weak<TerminalFd>>) {
        self.tty = val;
    }
}

#[derive(Debug)]
struct Config {
    path: PathBuf,
    connected_tx: Sender<OwnedFd>,
}

impl Config {
    fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl Terminal {
    /// Setup a new terminal instance.
    pub fn new(logger: SharedContainerLog, attach: SharedContainerAttach) -> Result<Self> {
        debug!("Creating new terminal");
        // The guard removes the socket again, also if the setup below fails.
        let temp_file = ContainerIO::temp_file(None, "conmon-term-", ".sock");
        let path = temp_file.path();

        // Bind synchronously so that the socket exists once this returns,
        // without blocking on the listener task to get scheduled.
        debug!("Listening terminal socket on {}", path.display());
        let listener = Listener::<DefaultListener>::default()
            .bind_long_path(path)
            .context("bind terminal socket")?;
        Self::restrict_permissions(path)?;

        let (connected_tx, connected_rx) = mpsc::channel(10);
        let config = Config {
            path: path.to_path_buf(),
            connected_tx,
        };

        task::spawn(
            async move {
                if let Err(e) = Self::listen(listener, config).await {
                    error!("Unable to listen on terminal: {:#}", e);
                };
            }
            .instrument(debug_span!("listen")),
        );

        let consumed = Arc::default();
        let (message_tx, message_rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        Ok(Self {
            path: temp_file,
            connected_rx,
            message_rx: Some(message_rx),
            message_tx: Some(StdioSender::new(message_tx, &consumed)),
            tty: None,
            logger,
            attach,
            consumed,
        })
    }

    /// Make the read loop wait for the consumer of the message channel.
    pub fn consume_stdio(&self) {
        self.consumed.store(true, Ordering::Release);
    }

    /// Make the terminal socket accessible to its owner only.
    fn restrict_permissions(path: &Path) -> Result<()> {
        let mut perms = std::fs::metadata(path)
            .context("get terminal socket metadata")?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(path, perms).context("set terminal socket permissions")
    }

    /// Waits for the socket client to be connected.
    pub async fn wait_connected(&mut self, stdin: bool, token: CancellationToken) -> Result<()> {
        let res = self.connect(stdin, token).await;
        if res.is_err() {
            // Nothing will ever read the stdin, so attach clients must not
            // wait for it.
            self.attach.close_stdin();
        }
        res
    }

    async fn connect(&mut self, stdin: bool, token: CancellationToken) -> Result<()> {
        debug!("Waiting for terminal socket connection");
        let fd = self
            .connected_rx
            .recv()
            .await
            .context("receive connected channel")?;
        let fd = Arc::new(TerminalFd::new(fd)?);
        self.set_tty(Arc::downgrade(&fd).into());

        debug!("Changing terminal settings");
        let mut term = termios::tcgetattr(&fd)?;
        term.output_flags |= OutputFlags::ONLCR;
        termios::tcsetattr(&fd, SetArg::TCSANOW, &term)?;

        let attach_clone = self.attach.clone();
        let output = attach_clone.output();
        let logger_clone = self.logger.clone();
        let message_tx = self.message_tx.take();

        task::spawn({
            let fd = fd.clone();
            async move {
                if let Err(e) = ContainerIO::read_loop(
                    &*fd,
                    Pipe::StdOut,
                    logger_clone,
                    message_tx,
                    attach_clone,
                    output,
                )
                .await
                {
                    error!("Stdout read loop failure: {:#}", e)
                }
                Ok::<_, anyhow::Error>(())
            }
            .instrument(debug_span!("read_loop"))
        });

        if stdin {
            let attach_clone = self.attach.clone();
            task::spawn(
                async move {
                    if let Err(e) = ContainerIO::read_loop_stdin(&*fd, attach_clone, token).await {
                        error!("Stdin read loop failure: {:#}", e);
                    }
                }
                .instrument(debug_span!("read_loop_stdin")),
            );
        } else {
            // Attach clients must not wait for a stdin nothing ever reads.
            self.attach.close_stdin();
        }

        Ok(())
    }

    /// The end of file character of the terminal, ^D unless the process
    /// changed it.
    pub fn eof_char(&self) -> u8 {
        const DEFAULT: u8 = 0x04;
        let Some(tty) = self.tty().as_ref().and_then(Weak::upgrade) else {
            return DEFAULT;
        };
        match termios::tcgetattr(&*tty) {
            Ok(term) => term.control_chars[SpecialCharacterIndices::VEOF as usize],
            Err(e) => {
                debug!("Unable to get the terminal settings: {e}");
                DEFAULT
            }
        }
    }

    /// Resize the terminal width and height.
    pub fn resize(&self, width: u16, height: u16) -> Result<()> {
        debug!("Resizing terminal to width {} and height {}", width, height);
        let ws = winsize {
            ws_row: height,
            ws_col: width,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let tty = self.tty().as_ref().and_then(Weak::upgrade);
        let tty = tty.context("terminal not connected")?;
        // SAFETY: TIOCSWINSZ reads a `winsize` from the pointer, which points
        // to a live local of exactly that type, and the descriptor is kept
        // open by the `Arc` for the duration of the call.
        match unsafe { libc::ioctl(tty.as_raw_fd(), TIOCSWINSZ, &ws) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error().into()),
        }
    }

    /// Accept the connection of the runtime and receive the terminal from it.
    /// The runtime never connects or never sends the descriptor if it fails
    /// early. The terminal gets dropped then, which closes the channel, so do
    /// not keep the listener around forever.
    async fn listen(listener: UnixListener, config: Config) -> Result<()> {
        let dropped = config.connected_tx.clone();
        select! {
            res = Self::accept(listener, config) => res,
            _ = dropped.closed() => {
                debug!("Terminal dropped before receiving a descriptor, stopping listener");
                Ok(())
            }
        }
    }

    async fn accept(listener: UnixListener, config: Config) -> Result<()> {
        let stream = listener.accept().await?.0;
        debug!("Got terminal socket stream: {:?}", stream);

        Self::handle_fd_receive(stream, config).await
    }

    async fn handle_fd_receive(mut stream: UnixStream, config: Config) -> Result<()> {
        let fd = match stream
            .async_io(Interest::READABLE, || Self::recv_fd(stream.as_fd()))
            .await
        {
            Ok(fd) => fd,
            Err(e) => {
                error!("Unable to receive data: {}", e);
                return Err(e.into());
            }
        };

        // Allow only one single read
        let path = config.path();
        debug!("Removing socket path {}", path.display());
        fs::remove_file(path).await?;

        debug!("Shutting down receiver stream");
        stream.shutdown().await?;

        if fd.is_none() {
            error!("No file descriptor received");
        }

        let fd = fd.context("got no file descriptor")?;

        debug!("Received terminal file descriptor");

        config
            .connected_tx
            .send(fd)
            .await
            .context("send connected channel")?;

        debug!("Shutting down listener thread");
        Ok(())
    }

    /// Receive the descriptor of the terminal. It gets close-on-exec right
    /// away, the runtimes which other threads spawn meanwhile would inherit
    /// the terminal of the container otherwise.
    fn recv_fd(socket: BorrowedFd) -> io::Result<Option<OwnedFd>> {
        // The kernel truncates a message with more descriptors than fit,
        // which nix reports as an error before they could be closed. One
        // message carries at most SCM_MAX_FD of them. A server at its limit
        // of open files gets a truncated message as well, the descriptors
        // received up to then leak.
        let mut cmsg_buffer = cmsg_space!([RawFd; 253]);
        let mut iov = [IoSliceMut::new(&mut [])];
        let msg = socket::recvmsg::<()>(
            socket.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg_buffer),
            MsgFlags::MSG_CMSG_CLOEXEC,
        )?;
        let mut fds = vec![];
        for cmsg in msg.cmsgs()? {
            if let ControlMessageOwned::ScmRights(raw_fds) = cmsg {
                // SAFETY: the descriptors were just received over the socket
                // and nothing else owns them, taking ownership right away
                // closes the ones which are not used.
                fds.extend(
                    raw_fds
                        .into_iter()
                        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
                );
            }
        }
        Ok(fds.into_iter().next())
    }
}

#[derive(Debug)]
struct TerminalFd(AsyncFd<std::fs::File>);

impl TerminalFd {
    fn new(fd: OwnedFd) -> io::Result<Self> {
        let flags = fcntl::fcntl(&fd, FcntlArg::F_GETFL)?;
        let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        fcntl::fcntl(&fd, FcntlArg::F_SETFL(flags))?;
        // SAFETY: `fd` is an open descriptor that the `File` takes ownership
        // of, so it stays valid and unchanged for the lifetime of the `AsyncFd`.
        Ok(Self(unsafe { AsyncFd::register(std::fs::File::from(fd)) }?))
    }
}

impl AsRawFd for TerminalFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsFd for TerminalFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsyncRead for &TerminalFd {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut ReadBuf,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;
            // Read into the uninitialized part directly, initializing it first
            // would zero the whole buffer on every read.
            // SAFETY: the slice is only handed to read(2), which writes to it
            // and never reads from it, nothing de-initializes it.
            let unfilled = unsafe { buf.unfilled_mut() };
            match guard.try_io(|inner| {
                // SAFETY: the pointer and length describe the writable slice
                // above, which outlives the call.
                let n = unsafe {
                    libc::read(
                        inner.as_raw_fd(),
                        unfilled.as_mut_ptr().cast(),
                        unfilled.len(),
                    )
                };
                usize::try_from(n).map_err(|_| io::Error::last_os_error())
            }) {
                Ok(n) => {
                    let n = n?;
                    // SAFETY: read(2) initialized the first `n` bytes.
                    unsafe { buf.assume_init(n) };
                    buf.advance(n);
                    break Poll::Ready(Ok(()));
                }
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for &TerminalFd {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.0.poll_write_ready(cx))?;
            match guard.try_io(|inner| inner.get_ref().write(buf)) {
                Ok(result) => break Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_log::ContainerLog;
    use nix::{fcntl::FdFlag, pty, sys::socket::ControlMessage};
    use std::{io::IoSlice, time::Duration};

    /// Send the terminal descriptor with some data, like a runtime does.
    async fn send_fd(stream: &UnixStream, data: &[u8], fd: BorrowedFd<'_>) -> Result<()> {
        let fds = [fd.as_raw_fd()];
        stream
            .async_io(Interest::WRITABLE, || {
                socket::sendmsg::<()>(
                    stream.as_raw_fd(),
                    &[IoSlice::new(data)],
                    &[ControlMessage::ScmRights(&fds)],
                    MsgFlags::empty(),
                    None,
                )
                .map_err(io::Error::from)
            })
            .await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_success() -> Result<()> {
        let logger = ContainerLog::new();
        let attach = SharedContainerAttach::default();
        let token = CancellationToken::new();

        let mut sut = Terminal::new(logger, attach)?;
        assert!(sut.path().exists());

        let res = pty::openpty(None, None)?;

        let stream = UnixStream::connect(sut.path()).await?;
        send_fd(&stream, b"test", res.master.as_fd()).await?;

        sut.wait_connected(true, token).await?;
        assert!(!sut.path().exists());

        // The received terminal does not leak into children.
        let tty = sut
            .tty()
            .as_ref()
            .and_then(Weak::upgrade)
            .context("no tty")?;
        let flags = FdFlag::from_bits_truncate(fcntl::fcntl(&*tty, FcntlArg::F_GETFD)?);
        assert!(flags.contains(FdFlag::FD_CLOEXEC));

        sut.resize(120, 40)?;
        let mut size = winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCGWINSZ writes a `winsize` to the pointer, which points
        // to a live local of exactly that type.
        assert_eq!(
            unsafe { libc::ioctl(res.slave.as_raw_fd(), libc::TIOCGWINSZ, &mut size) },
            0
        );
        assert_eq!((size.ws_col, size.ws_row), (120, 40));

        // Write to the slave
        let mut file: std::fs::File = res.slave.into();
        file.write_all(b"test")?;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eof_char_ends_the_input() -> Result<()> {
        let attach = SharedContainerAttach::default();
        let mut sut = Terminal::new(ContainerLog::new(), attach.clone())?;
        let res = pty::openpty(None, None)?;
        let stream = UnixStream::connect(sut.path()).await?;
        send_fd(&stream, b"x", res.master.as_fd()).await?;
        sut.wait_connected(true, CancellationToken::new()).await?;

        let eof = sut.eof_char();
        assert_eq!(eof, 0x04);
        attach.send_stdin(vec![eof].into()).await;

        // A process reading the terminal sees the end of its input.
        let mut slave: std::fs::File = res.slave.into();
        let read = task::spawn_blocking(move || io::Read::read(&mut slave, &mut [0; 16]));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), read).await???,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn listen_stops_without_terminal() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("term.sock");
        let listener = UnixListener::bind(&path)?;
        let (connected_tx, connected_rx) = mpsc::channel(1);
        let config = Config { path, connected_tx };

        // A runtime which connects but never sends the descriptor must not
        // keep the listener around either once the terminal side is gone.
        let listen = tokio::spawn(Terminal::listen(listener, config));
        let _stream = UnixStream::connect(dir.path().join("term.sock")).await?;
        drop(connected_rx);
        tokio::time::timeout(Duration::from_secs(5), listen).await???;
        Ok(())
    }
}
