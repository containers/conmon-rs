//! Terminal console functionalities.

use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, Message, Pipe},
    container_log::SharedContainerLog,
    listener::{DefaultListener, Listener},
};
use anyhow::{Context as _, Result, format_err};
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
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Weak, mpsc::Sender as StdSender},
    task::{Context, Poll, ready},
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, Interest, ReadBuf, unix::AsyncFd},
    net::UnixStream,
    sync::mpsc::{self, Receiver, Sender},
    task,
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, trace};

#[derive(Debug)]
pub struct Terminal {
    path: PathBuf,
    connected_rx: Receiver<OwnedFd>,
    message_rx: Option<UnboundedReceiver<Message>>,
    tty: Option<Weak<TerminalFd>>,
    logger: SharedContainerLog,
    attach: SharedContainerAttach,
}

impl Terminal {
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
    pub fn message_rx(&self) -> &Option<UnboundedReceiver<Message>> {
        &self.message_rx
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
    ready_tx: StdSender<()>,
    connected_tx: Sender<OwnedFd>,
}

impl Config {
    fn path(&self) -> &PathBuf {
        &self.path
    }
    fn ready_tx(&self) -> &StdSender<()> {
        &self.ready_tx
    }
}

impl Terminal {
    /// Setup a new terminal instance.
    pub fn new(logger: SharedContainerLog, attach: SharedContainerAttach) -> Result<Self> {
        debug!("Creating new terminal");
        let path = ContainerIO::temp_file_name(None, "conmon-term-", ".sock");
        let path_clone = path.clone();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (connected_tx, connected_rx) = mpsc::channel(10);

        task::spawn(
            async move {
                if let Err(e) = Self::listen(Config {
                    path: path_clone,
                    ready_tx,
                    connected_tx,
                })
                .await
                {
                    error!("Unable to listen on terminal: {:#}", e);
                };
            }
            .instrument(debug_span!("listen")),
        );
        ready_rx.recv().context("wait for listener to be ready")?;

        Ok(Self {
            path,
            connected_rx,
            message_rx: None,
            tty: None,
            logger,
            attach,
        })
    }

    /// Waits for the socket client to be connected.
    pub async fn wait_connected(&mut self, stdin: bool, token: CancellationToken) -> Result<()> {
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
                    message_tx,
                    attach_clone,
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
        match unsafe {
            libc::ioctl(
                tty.context("terminal not connected")?.as_raw_fd(),
                TIOCSWINSZ,
                &ws,
            )
        } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error().into()),
        }
    }

    async fn listen(config: Config) -> Result<()> {
        let path = config.path();
        debug!("Listening terminal socket on {}", path.display());
        let listener = Listener::<DefaultListener>::default().bind_long_path(path)?;

        // Update the permissions
        let mut perms = fs::metadata(path).await?.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(path, perms).await?;

        config
            .ready_tx()
            .send(())
            .map_err(|_| format_err!("unable to send ready message"))?;

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

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(self.path()) {
            trace!(
                "Unable to remove socket file path {}: {}",
                self.path().display(),
                e
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_log::ContainerLog;
    use nix::pty;
    use sendfd::SendWithFd;

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
}

#[derive(Debug)]
struct TerminalFd(AsyncFd<std::fs::File>);

impl TerminalFd {
    fn new(fd: OwnedFd) -> io::Result<Self> {
        let flags = fcntl::fcntl(&fd, FcntlArg::F_GETFL)?;
        let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        fcntl::fcntl(&fd, FcntlArg::F_SETFL(flags))?;
        AsyncFd::new(fd.into()).map(Self)
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
