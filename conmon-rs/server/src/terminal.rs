//! Terminal console functionalities.

use crate::{
    container_io::Message,
    container_log::{Pipe, SharedContainerLog},
};
use anyhow::{bail, format_err, Context, Result};
use getset::{Getters, MutGetters};
use log::{debug, error, trace};
use nix::{
    errno::Errno,
    sys::termios::{self, OutputFlags, SetArg},
};
use sendfd::RecvWithFd;
use std::{
    io::ErrorKind,
    os::unix::{
        fs::PermissionsExt,
        io::{FromRawFd, RawFd},
    },
    path::{Path, PathBuf},
    str,
};
use tempfile::Builder;
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncWriteExt, BufReader, Interest},
    net::UnixStream,
    select,
    sync::{
        broadcast,
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot::{self, Sender},
    },
    task,
};

#[derive(Debug, Getters, MutGetters)]
pub struct Terminal {
    #[getset(get = "pub")]
    path: PathBuf,

    connected_rx: UnboundedReceiver<()>,

    #[getset(get = "pub", get_mut = "pub")]
    message_rx: UnboundedReceiver<Message>,

    #[getset(get = "pub")]
    attach_tx: UnboundedSender<(broadcast::Receiver<String>, broadcast::Sender<Vec<u8>>)>,
}

impl Terminal {
    /// Setup a new terminal instance.
    pub async fn new(logger: SharedContainerLog) -> Result<Self> {
        debug!("Creating new terminal");
        let path = Self::temp_file_name(None, "conmon-term-", ".sock")?;
        let path_clone = path.clone();

        let (ready_tx, ready_rx) = oneshot::channel();
        let (connected_tx, connected_rx) = mpsc::unbounded_channel();
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        let (attach_tx, attach_rx) = mpsc::unbounded_channel();

        task::spawn(async move {
            Self::listen(
                path_clone,
                ready_tx,
                connected_tx,
                message_tx,
                attach_rx,
                logger,
            )
            .await
        });
        ready_rx.await.context("wait for listener to be ready")?;

        Ok(Self {
            path,
            connected_rx,
            message_rx,
            attach_tx,
        })
    }

    /// Waits for the socket client to be connected.
    pub async fn wait_connected(&mut self) -> Result<()> {
        debug!("Waiting for terminal socket connection");
        self.connected_rx
            .recv()
            .await
            .context("receive connected channel")
    }

    /// Generate a the temp file name without creating the file.
    pub fn temp_file_name(directory: Option<&Path>, prefix: &str, suffix: &str) -> Result<PathBuf> {
        let mut file = Builder::new();
        file.prefix(prefix).suffix(suffix).rand_bytes(7);
        let file = match directory {
            Some(d) => file.tempfile_in(d),
            None => file.tempfile(),
        }
        .context("create tempfile")?;

        let path: PathBuf = file.path().into();
        drop(file);
        Ok(path)
    }

    async fn listen(
        path: PathBuf,
        ready_tx: Sender<()>,
        connected_tx: UnboundedSender<()>,
        message_tx: UnboundedSender<Message>,
        attach_rx: UnboundedReceiver<(broadcast::Receiver<String>, broadcast::Sender<Vec<u8>>)>,
        logger: SharedContainerLog,
    ) -> Result<()> {
        debug!("Listening terminal socket on {}", path.display());
        let listener = crate::listener::bind_long_path(&path)?;

        // Update the permissions
        let mut perms = fs::metadata(&path).await?.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&path, perms).await?;

        ready_tx
            .send(())
            .map_err(|_| format_err!("unable to send ready message"))?;

        let stream = listener.accept().await?.0;
        debug!("Got terminal socket stream: {:?}", stream);

        Self::handle_fd_receive(stream, path, connected_tx, message_tx, attach_rx, logger).await
    }

    async fn handle_fd_receive(
        mut stream: UnixStream,
        path: PathBuf,
        connected_tx: UnboundedSender<()>,
        message_tx: UnboundedSender<Message>,
        attach_rx: UnboundedReceiver<(broadcast::Receiver<String>, broadcast::Sender<Vec<u8>>)>,
        logger: SharedContainerLog,
    ) -> Result<()> {
        loop {
            if !stream.ready(Interest::READABLE).await?.is_readable() {
                continue;
            }

            let mut data_buffer = [];
            let mut fd_buffer: [RawFd; 1] = [0];

            match stream.recv_with_fd(&mut data_buffer, &mut fd_buffer) {
                Ok((_, fd_read)) => {
                    // Allow only one single read
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

                    task::spawn(async move {
                        Self::read_loop(fd, connected_tx, message_tx, attach_rx, logger).await
                    });

                    // TODO: Now that we have a fd to the tty, make sure we handle any pending
                    // data that was already buffered.
                    // See: https://github.com/containers/conmon/blob/f263cf4/src/ctrl.c#L68

                    // TODO: Now that we've set mainfd_stdout, we can register the
                    // ctrl_winsz_cb if we didn't set it here, we'd risk attempting to run
                    // ioctl on a negative fd, and fail to resize the window.
                    // See: https://github.com/containers/conmon/blob/f263cf4/src/ctrl.c#L73

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

    async fn read_loop(
        fd: RawFd,
        connected_tx: UnboundedSender<()>,
        message_tx: UnboundedSender<Message>,
        mut attach_rx: UnboundedReceiver<(broadcast::Receiver<String>, broadcast::Sender<Vec<u8>>)>,
        logger: SharedContainerLog,
    ) -> Result<()> {
        debug!("Start reading from file descriptor");

        let file = unsafe { File::from_raw_fd(fd) };
        let mut reader = BufReader::new(file);
        let mut writer = unsafe { File::from_raw_fd(fd) };
        let mut buf = vec![0; 1024];

        connected_tx
            .send(())
            .map_err(|_| format_err!("unable to send connected message"))?;

        let mut attach_stdout = None;
        let mut attach_stdin: Option<broadcast::Receiver<String>> = None;

        loop {
            if let Some(stdin) = attach_stdin.as_mut() {
                let data = stdin.recv().await?;
                writer.write_all(data.as_bytes()).await?;
                // TODO: make this work correctly in parallel
                attach_stdin = None;
            }
            select! {
                res = attach_rx.recv() => if let Some((new_stdin, new_stdout)) = res {
                    debug!("Got new attach to terminal");
                    attach_stdin = new_stdin.into();
                    attach_stdout = new_stdout.into();
                },
                res = reader.read(&mut buf) => {
                    match res {
                        Ok(n) if n > 0 => {
                            debug!("Read {} bytes", n);
                            let data = &buf[..n];

                            let mut locked_logger = logger.write().await;
                            locked_logger
                                .write(Pipe::StdOut, data)
                                .await
                                .context("write to log file")?;

                            if let Some(stdout) = attach_stdout.as_ref() {
                                stdout.send(data.to_vec())?;
                            }

                            message_tx
                                .send(Message::Data(data.into()))
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
                },
            };
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
    use std::{os::unix::io::FromRawFd, sync::Arc};
    use tokio::sync::RwLock;

    #[tokio::test]
    async fn new_success() -> Result<()> {
        let logger = Arc::new(RwLock::new(ContainerLog::default()));

        let mut sut = Terminal::new(logger).await?;
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

        sut.wait_connected().await?;
        assert!(!sut.path().exists());

        // Write to the slave
        let mut file = unsafe { fs::File::from_raw_fd(res.slave) };
        file.write_all(b"test").await?;

        Ok(())
    }
}
