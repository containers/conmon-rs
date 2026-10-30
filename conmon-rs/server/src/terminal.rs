//! Terminal console functionalities.

use crate::{
    async_fd::{self, Fd},
    container_io::{ContainerIO, TempFile},
    listener,
};
use anyhow::{Context as _, Result};
use libc::{TIOCSWINSZ, winsize};
use nix::{
    cmsg_space,
    fcntl::{self, FcntlArg, OFlag},
    sys::{
        socket::{self, Backlog, ControlMessageOwned, MsgFlags, SockType, UnixAddr},
        termios::{self, OutputFlags, SetArg, SpecialCharacterIndices},
    },
};
use std::{
    io::{self, IoSliceMut, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::{io::RawFd, net::UnixListener as StdUnixListener},
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
use tracing::{Instrument, debug, debug_span, error};

#[derive(Debug)]
pub struct Terminal {
    /// The socket the runtime connects to, removed with the terminal.
    path: TempFile,
    connected_rx: Receiver<OwnedFd>,

    /// The terminal once connected, as long as the loops which read and
    /// write it keep it open.
    tty: Option<Weak<TerminalFd>>,
}

impl Terminal {
    pub fn path(&self) -> &Path {
        self.path.path()
    }
}

#[derive(Debug)]
struct Config {
    path: PathBuf,
    connected_tx: Sender<OwnedFd>,
}

impl Terminal {
    /// Setup a new terminal instance.
    pub fn new() -> Result<Self> {
        debug!("Creating new terminal");
        // The guard removes the socket again, also if the setup below fails.
        // In the temporary directory, not the runtime directory of the server:
        // runc dials the console socket by its path, which a Unix socket
        // address limits to 108 bytes, and the runtime directory of CRI-O is
        // far too long for that.
        let temp_file = ContainerIO::temp_file(None, "conmon-term-", ".sock");
        let path = temp_file.path();

        // Bind synchronously so that the socket exists once this returns,
        // without blocking on the listener task to get scheduled.
        debug!("Listening terminal socket on {}", path.display());
        let listener = listener::bind_long_path(path, |path| {
            let fd = listener::private_socket(SockType::Stream)?;
            socket::bind(fd.as_raw_fd(), &UnixAddr::new(&path)?)?;
            socket::listen(&fd, Backlog::MAXCONN)?;
            UnixListener::from_std(StdUnixListener::from(fd))
        })
        .context("bind terminal socket")?;

        // It carries the single descriptor of the terminal.
        let (connected_tx, connected_rx) = mpsc::channel(1);
        let config = Config {
            path: path.to_path_buf(),
            connected_tx,
        };

        task::spawn(
            async move {
                if let Err(e) = Self::listen(listener, config).await {
                    error!("Unable to listen on terminal: {e:#}");
                }
            }
            .instrument(debug_span!("listen")),
        );

        Ok(Self {
            path: temp_file,
            connected_rx,
            tty: None,
        })
    }

    /// Wait for the runtime to hand over the terminal, for the loops which
    /// read and write it, see [`ContainerIO::connect_terminal`]. A wait which
    /// gets dropped leaves the descriptor queued for the next one.
    pub async fn connect(&mut self) -> Result<TerminalIo> {
        debug!("Waiting for terminal socket connection");
        let fd = self
            .connected_rx
            .recv()
            .await
            .context("receive connected channel")?;
        let fd = Arc::new(TerminalFd::new(fd)?);
        self.tty = Some(Arc::downgrade(&fd));

        debug!("Changing terminal settings");
        let mut term = termios::tcgetattr(&fd)?;
        term.output_flags |= OutputFlags::ONLCR;
        termios::tcsetattr(&fd, SetArg::TCSANOW, &term)?;
        Ok(TerminalIo(fd))
    }

    /// The end of file character of the terminal, ^D unless the process
    /// changed it.
    pub fn eof_char(&self) -> u8 {
        const DEFAULT: u8 = 0x04;
        let Some(tty) = self.tty.as_ref().and_then(Weak::upgrade) else {
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
    #[expect(unsafe_code, reason = "the TIOCSWINSZ ioctl")]
    pub fn resize(&self, width: u16, height: u16) -> Result<()> {
        debug!("Resizing terminal to width {} and height {}", width, height);
        let ws = winsize {
            ws_row: height,
            ws_col: width,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let tty = self.tty.as_ref().and_then(Weak::upgrade);
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
            () = dropped.closed() => {
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

        // Allow only one single connection. Failing to clean up must not
        // drop the terminal, which the runtime handed over already.
        let path = &config.path;
        debug!("Removing socket path {}", path.display());
        if let Err(e) = fs::remove_file(path).await {
            debug!("Unable to remove terminal socket: {e}");
        }
        debug!("Shutting down receiver stream");
        if let Err(e) = stream.shutdown().await {
            debug!("Unable to shut down terminal socket stream: {e}");
        }

        let fd = fd.context("got no file descriptor")?;
        debug!("Received terminal file descriptor");
        config
            .connected_tx
            .send(fd)
            .await
            .context("send connected channel")?;

        debug!("Shutting down listener task");
        Ok(())
    }

    /// Receive the descriptor of the terminal. It gets close-on-exec right
    /// away, the runtimes which other threads spawn meanwhile would inherit
    /// the terminal of the container otherwise.
    #[expect(unsafe_code, reason = "taking received descriptors")]
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
struct TerminalFd(AsyncFd<Fd<std::fs::File>>);

impl TerminalFd {
    fn new(fd: OwnedFd) -> io::Result<Self> {
        let flags = fcntl::fcntl(&fd, FcntlArg::F_GETFL)?;
        let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        fcntl::fcntl(&fd, FcntlArg::F_SETFL(flags))?;
        let file = std::fs::File::from(fd);
        Ok(Self(async_fd::register(
            file,
            Interest::READABLE | Interest::WRITABLE,
        )?))
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

/// The terminal for the loops which read and write it, which keep it open.
#[derive(Clone, Debug)]
pub struct TerminalIo(Arc<TerminalFd>);

impl AsyncRead for TerminalIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut ReadBuf,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.0.0.poll_read_ready(cx))?;
            match guard.try_io(|inner| (&**inner.get_ref()).read(buf.initialize_unfilled())) {
                Ok(result) => {
                    buf.advance(result?);
                    break Poll::Ready(Ok(()));
                }
                Err(_would_block) => {}
            }
        }
    }
}

impl AsyncWrite for TerminalIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.0.0.poll_write_ready(cx))?;
            match guard.try_io(|inner| (&**inner.get_ref()).write(buf)) {
                Ok(result) => break Poll::Ready(result),
                Err(_would_block) => {}
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
mod tests;
