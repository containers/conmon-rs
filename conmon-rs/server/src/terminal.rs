//! Terminal console functionalities.

use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, Message, Pipe},
    container_log::SharedContainerLog,
    listener::{DefaultListener, Listener},
};
use anyhow::{bail, format_err, Context, Result};
use getset::{Getters, MutGetters, Setters};
use libc::{self, winsize, TIOCSWINSZ};
use nix::sys::termios::{self, OutputFlags, SetArg};
use sendfd::RecvWithFd;
use std::{
    convert::TryFrom,
    io::{Error as IOError, ErrorKind},
    os::unix::{fs::PermissionsExt, io::RawFd},
    path::PathBuf,
    sync::mpsc::Sender as StdSender,
};
use tokio::{
    fs,
    io::{AsyncWriteExt, Interest},
    net::UnixStream,
    sync::mpsc::{self, Receiver, Sender},
    task,
};
use tokio_fd::AsyncFd;
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, error, trace, Instrument};

#[derive(Debug, Getters, MutGetters, Setters)]
pub struct Terminal {
    #[getset(get = "pub")]
    path: PathBuf,

    connected_rx: Receiver<RawFd>,

    #[getset(get = "pub", get_mut = "pub")]
    message_rx: Option<Receiver<Message>>,

    #[getset(get, set)]
    tty: Option<RawFd>,

    logger: SharedContainerLog,
    attach: SharedContainerAttach,
}

#[derive(Debug, Getters)]
struct Config {
    #[get]
    path: PathBuf,

    #[get]
    ready_tx: StdSender<()>,

    #[get]
    connected_tx: Sender<RawFd>,
}

impl Terminal {
    /// Setup a new terminal instance.
    pub fn new(logger: SharedContainerLog, attach: SharedContainerAttach) -> Result<Self> {
        debug!("Creating new terminal");
        let path = ContainerIO::temp_file_name(None, "conmon-term-", ".sock")?;
        let path_clone = path.clone();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (connected_tx, connected_rx) = mpsc::channel(1);

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
        self.set_tty(fd.into());

        debug!("Changing terminal settings");
        let mut term = termios::tcgetattr(fd)?;
        term.output_flags |= OutputFlags::ONLCR;
        termios::tcsetattr(fd, SetArg::TCSANOW, &term)?;

        let stdio = AsyncFd::try_from(fd)?;

        let attach_clone = self.attach.clone();
        let logger_clone = self.logger.clone();
        let (message_tx, message_rx) = ContainerIO::message_channel();
        self.message_rx = Some(message_rx);

        task::spawn(
            async move {
                if let Err(e) = ContainerIO::read_loop(
                    stdio,
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
            .instrument(debug_span!("read_loop")),
        );

        if stdin {
            let attach_clone = self.attach.clone();
            task::spawn(
                async move {
                    if let Err(e) = ContainerIO::read_loop_stdin(fd, attach_clone, token).await {
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
        match unsafe {
            libc::ioctl(
                self.tty().context("terminal not connected")?,
                TIOCSWINSZ,
                &ws,
            )
        } {
            0 => Ok(()),
            _ => Err(IOError::last_os_error().into()),
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
                    // Allow only one single read
                    let path = config.path();
                    debug!("Removing socket path {}", path.display());
                    fs::remove_file(path).await?;

                    debug!("Shutting down receiver stream");
                    stream.shutdown().await?;

                    if fd_read == 0 {
                        error!("No file descriptor received");
                        bail!("got no file descriptor");
                    }

                    debug!("Received terminal file descriptor");
                    let fd = fd_buffer[0];

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
    use crate::{attach::SharedContainerAttach, container_log::ContainerLog};
    use nix::pty;
    use sendfd::SendWithFd;
    use std::os::unix::io::FromRawFd;

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
                match stream.send_with_fd(b"test", &[res.master]) {
                    Ok(_) => break,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Err(e) => bail!(e),
                }
            }
        }

        sut.wait_connected(true, token).await?;
        assert!(!sut.path().exists());

        // Write to the slave
        let mut file = unsafe { fs::File::from_raw_fd(res.slave) };
        file.write_all(b"test").await?;

        Ok(())
    }
}
