//! Console socket functionalities.

use crate::iostreams::IOStreams;
use anyhow::{bail, format_err, Context, Result};
use getset::Getters;
use log::{debug, error, trace};
use nix::sys::termios::{self, OutputFlags, SetArg};
use sendfd::RecvWithFd;
use std::{
    io::ErrorKind,
    os::unix::{fs::PermissionsExt, io::RawFd},
    path::{Path, PathBuf},
    str,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};
use tempfile::Builder;
use tokio::{
    fs,
    io::{AsyncWriteExt, Interest},
    net::{UnixListener, UnixStream},
    runtime::Runtime,
};

#[derive(Debug, Getters)]
pub struct Console {
    #[getset(get = "pub")]
    path: PathBuf,

    connected_rx: Receiver<()>,
}

impl Console {
    /// Setup a new console socket.
    pub fn new() -> Result<Self> {
        debug!("Creating new console socket");
        let path = Self::temp_file_name("conmon-term-", ".sock")?;
        let path_clone = path.clone();

        let (ready_tx, ready_rx) = mpsc::channel();
        let (connected_tx, connected_rx) = mpsc::channel();

        thread::spawn(move || Self::listen(&path_clone, ready_tx, connected_tx));
        ready_rx.recv().context("wait for listener to be ready")?;

        Ok(Self { path, connected_rx })
    }

    /// Waits for the socket client to be connected.
    pub fn wait_connected(&self) -> Result<()> {
        debug!("Waiting for console socket connection");
        self.connected_rx
            .recv_timeout(Duration::from_secs(60))
            .context("receive connected channel")
    }

    #[allow(dead_code)]
    /// Create window resize control FIFO.
    pub fn setup_fifo() -> Result<()> {
        // TODO: implement me
        unimplemented!();
    }

    /// Generate a the temp file name without creating the file.
    fn temp_file_name(prefix: &str, suffix: &str) -> Result<PathBuf> {
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

    fn listen(path: &Path, ready_tx: Sender<()>, connected_tx: Sender<()>) -> Result<()> {
        Runtime::new()?.block_on(async move {
            let listener = UnixListener::bind(&path)?;
            debug!("Listening console socket on {}", &path.display());

            // Update the permissions
            let mut perms = fs::metadata(&path).await?.permissions();
            perms.set_mode(0o700);
            fs::set_permissions(&path, perms).await?;

            ready_tx
                .send(())
                .map_err(|_| format_err!("unable to send ready message"))?;

            let stream = listener.accept().await?.0;
            debug!("Got console socket stream: {:?}", stream);

            Self::handle_fd_receive(stream, path, connected_tx).await
        })
    }

    async fn handle_fd_receive(
        mut stream: UnixStream,
        path: &Path,
        connected_tx: Sender<()>,
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
                    debug!("Removing socket path {}", &path.display());
                    fs::remove_file(&path).await?;

                    debug!("Shutting down receiver stream");
                    stream.shutdown().await?;

                    if fd_read == 0 {
                        error!("No file descriptor received");
                        bail!("got no file descriptor");
                    }

                    debug!("Received console file descriptor");
                    let fd = fd_buffer[0];

                    debug!("Changing terminal settings");
                    let mut term = termios::tcgetattr(fd)?;
                    term.output_flags |= OutputFlags::ONLCR;
                    termios::tcsetattr(fd, SetArg::TCSANOW, &term)?;

                    IOStreams::from_raw_fd(fd)?.start()?;

                    // TODO: Now that we have a fd to the tty, make sure we handle any pending
                    // data that was already buffered.
                    // See: https://github.com/containers/conmon/blob/f263cf4/src/ctrl.c#L68

                    // TODO: Now that we've set mainfd_stdout, we can register the
                    // ctrl_winsz_cb if we didn't set it here, we'd risk attempting to run
                    // ioctl on a negative fd, and fail to resize the window.
                    // See: https://github.com/containers/conmon/blob/f263cf4/src/ctrl.c#L73

                    connected_tx.send(()).context("send connected channel")?;
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

impl Drop for Console {
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
        let sut = Console::new()?;
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
        file.write(b"test").await?;

        Ok(())
    }
}
