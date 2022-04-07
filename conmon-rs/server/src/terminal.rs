//! Terminal console functionalities.

use crate::{container_io::Message, stream::Stream};
use anyhow::{bail, format_err, Context, Result};
use getset::Getters;
use log::{debug, error, trace};
use nix::{
    errno::Errno,
    sys::termios::{self, OutputFlags, SetArg},
};
use sendfd::RecvWithFd;
use std::{
    io::{BufReader, ErrorKind, Read},
    os::unix::{fs::PermissionsExt, io::RawFd},
    path::PathBuf,
    str,
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
};
use tempfile::Builder;
use tokio::{
    fs,
    io::{AsyncWriteExt, Interest},
    net::{UnixListener, UnixStream},
    task,
};

#[derive(Debug, Getters)]
pub struct Terminal {
    #[getset(get = "pub")]
    path: PathBuf,

    connected_rx: Receiver<()>,

    #[getset(get = "pub")]
    message_rx: Receiver<Message>,
}

#[derive(Debug, Getters)]
struct Config {
    #[get]
    path: PathBuf,

    #[get]
    ready_tx: Sender<()>,

    #[get]
    connected_tx: Sender<()>,

    #[get]
    message_tx: Sender<Message>,
}

impl Terminal {
    /// Setup a new terminal instance.
    pub fn new() -> Result<Self> {
        debug!("Creating new terminal");
        let path = Self::temp_file_name("conmon-term-", ".sock")?;
        let path_clone = path.clone();

        let (ready_tx, ready_rx) = mpsc::channel();
        let (connected_tx, connected_rx) = mpsc::channel();
        let (message_tx, message_rx) = mpsc::channel();

        task::spawn(async move {
            Self::listen(Config {
                path: path_clone,
                ready_tx,
                connected_tx,
                message_tx,
            })
            .await
        });
        ready_rx.recv().context("wait for listener to be ready")?;

        Ok(Self {
            path,
            connected_rx,
            message_rx,
        })
    }

    /// Waits for the socket client to be connected.
    pub fn wait_connected(&self) -> Result<()> {
        debug!("Waiting for terminal socket connection");
        self.connected_rx
            .recv_timeout(Duration::from_secs(60))
            .context("receive connected channel")
    }

    /// Generate a the temp file name without creating the file.
    pub fn temp_file_name(prefix: &str, suffix: &str) -> Result<PathBuf> {
        let file = Builder::new()
            .prefix(prefix)
            .suffix(suffix)
            .rand_bytes(7)
            .tempfile()
            .context("create tempfile")?;
        let path: PathBuf = file.path().into();
        drop(file);
        Ok(path)
    }

    async fn listen(config: Config) -> Result<()> {
        let path = config.path();
        debug!("Listening terminal socket on {}", path.display());
        let listener = UnixListener::bind(path)?;

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

                    debug!("Changing terminal settings");
                    let mut term = termios::tcgetattr(fd)?;
                    term.output_flags |= OutputFlags::ONLCR;
                    termios::tcsetattr(fd, SetArg::TCSANOW, &term)?;

                    let message_tx = config.message_tx().clone();
                    task::spawn_blocking(move || Self::read_loop(fd, message_tx));

                    // TODO: Now that we have a fd to the tty, make sure we handle any pending
                    // data that was already buffered.
                    // See: https://github.com/containers/conmon/blob/f263cf4/src/ctrl.c#L68

                    // TODO: Now that we've set mainfd_stdout, we can register the
                    // ctrl_winsz_cb if we didn't set it here, we'd risk attempting to run
                    // ioctl on a negative fd, and fail to resize the window.
                    // See: https://github.com/containers/conmon/blob/f263cf4/src/ctrl.c#L73

                    config
                        .connected_tx()
                        .send(())
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

    fn read_loop(fd: RawFd, message_tx: Sender<Message>) -> Result<()> {
        debug!("Start reading from file descriptor");

        let stream = Stream::from(fd);
        let mut reader = BufReader::new(stream);
        let mut buf = vec![0; 1024];

        loop {
            match reader.read(&mut buf) {
                Ok(n) if n > 0 => {
                    debug!("Read {} bytes", n);

                    message_tx
                        .send(Message::Data((&buf[..n]).into()))
                        .context("send data message")?;
                }
                Err(e) => match Errno::from_i32(e.raw_os_error().context("get OS error")?) {
                    Errno::EIO => {
                        debug!("Stopping terminal read loop");
                        message_tx
                            .send(Message::Done)
                            .context("send done message")?;
                        return Ok(());
                    }
                    _ => error!("Unable to read from file descriptor: {}", e),
                },
                _ => {}
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
    use nix::pty;
    use sendfd::SendWithFd;
    use std::os::unix::io::FromRawFd;

    #[tokio::test]
    async fn new_success() -> Result<()> {
        let sut = Terminal::new()?;
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

        sut.wait_connected()?;
        assert!(!sut.path().exists());

        // Write to the slave
        let mut file = unsafe { fs::File::from_raw_fd(res.slave) };
        file.write_all(b"test").await?;

        Ok(())
    }
}
