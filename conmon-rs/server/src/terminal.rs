//! Terminal console functionalities.

use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, Message, Pipe, TempFile},
    container_log::SharedContainerLog,
    listener::{DefaultListener, Listener},
};
use anyhow::{Context as _, Result};
use async_channel::Receiver as UnboundedReceiver;
use libc::{TIOCSWINSZ, winsize};
use nix::{
    fcntl::{self, FcntlArg, OFlag},
    sys::termios::{self, OutputFlags, SetArg},
};
use sendfd::RecvWithFd;
use std::{
    io::{self, ErrorKind, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::{fs::PermissionsExt, io::RawFd},
    },
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Weak},
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
use tracing::{Instrument, debug, debug_span, error, trace};

#[derive(Debug)]
pub struct Terminal {
    /// The socket the runtime connects to, removed with the terminal.
    path: TempFile,
    connected_rx: Receiver<OwnedFd>,
    message_rx: Option<UnboundedReceiver<Message>>,
    tty: Option<Weak<TerminalFd>>,
    logger: SharedContainerLog,
    attach: SharedContainerAttach,
}

impl Terminal {
    pub fn path(&self) -> &Path {
        self.path.path()
    }
    pub fn message_rx(&self) -> Option<&UnboundedReceiver<Message>> {
        self.message_rx.as_ref()
    }
    pub fn message_rx_mut(&mut self) -> &mut Option<UnboundedReceiver<Message>> {
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

        Ok(Self {
            path: temp_file,
            connected_rx,
            message_rx: None,
            tty: None,
            logger,
            attach,
        })
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
        let (message_tx, message_rx) = async_channel::bounded(100);
        self.message_rx = Some(message_rx);

        task::spawn({
            let fd = fd.clone();
            async move {
                if let Err(e) = ContainerIO::read_loop(
                    &*fd,
                    Pipe::StdOut,
                    logger_clone,
                    Some(message_tx),
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
        loop {
            if !stream.ready(Interest::READABLE).await?.is_readable() {
                continue;
            }

            let mut data_buffer = [];
            let mut fd_buffer: [RawFd; 1] = [0];

            match stream.recv_with_fd(&mut data_buffer, &mut fd_buffer) {
                Ok((_, fd_read)) => {
                    // take ownership of the received file descriptor (prevents fd leak in case of error)
                    let fd = (fd_read != 0).then(|| unsafe { OwnedFd::from_raw_fd(fd_buffer[0]) });

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
                    return Ok(());
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    trace!("WouldBlock error, retrying");
                    continue;
                }
                Err(e) => {
                    error!("Unable to receive data: {}", e);
                    return Err(e.into());
                }
            }
        }
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
            match guard.try_io(|inner| inner.get_ref().read(buf.initialize_unfilled())) {
                Ok(n) => {
                    buf.advance(n?);
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
    use nix::pty;
    use sendfd::SendWithFd;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread")]
    async fn new_success() -> Result<()> {
        let logger = ContainerLog::new();
        let attach = SharedContainerAttach::default();
        let token = CancellationToken::new();

        let mut sut = Terminal::new(logger, attach)?;
        assert!(sut.path().exists());

        let res = pty::openpty(None, None)?;

        let stream = UnixStream::connect(sut.path()).await?;
        loop {
            let ready = stream.ready(Interest::WRITABLE).await?;
            if ready.is_writable() {
                match stream.send_with_fd(b"test", &[res.master.as_raw_fd()]) {
                    Ok(_) => break,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Err(e) => anyhow::bail!(e),
                }
            }
        }

        sut.wait_connected(true, token).await?;
        assert!(!sut.path().exists());

        // Write to the slave
        let mut file: std::fs::File = res.slave.into();
        file.write_all(b"test")?;

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
