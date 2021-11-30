//! Console socket functionalities.

#![allow(dead_code)] // TODO: remove me when actually used

use anyhow::{bail, format_err, Context, Result};
use getset::Getters;
use log::{debug, error, trace};
use nix::sys::termios::{self, OutputFlags, SetArg};
use sendfd::RecvWithFd;
use std::{
    io::ErrorKind,
    os::unix::{fs::PermissionsExt, io::RawFd},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    thread::{self, JoinHandle},
};
use tempfile::Builder;
use tokio::{
    fs,
    io::{AsyncWriteExt, Interest},
    net::{UnixListener, UnixStream},
    runtime::Runtime,
};

#[derive(Debug, Getters)]
#[getset(get)]
pub struct Console {
    path: PathBuf,
    handle: JoinHandle<Result<()>>,
    fd: Arc<RwLock<Option<RawFd>>>,
}

impl Console {
    /// Setup a new console socket.
    pub fn new() -> Result<Self> {
        let path = Self::temp_file_name("conmon-term-", ".sock")?;
        let fd = Arc::new(RwLock::new(None));
        let fd_clone = fd.clone();
        let path_clone = path.clone();
        let handle = thread::spawn(move || Self::listen(&path_clone, fd_clone));
        Ok(Self { path, handle, fd })
    }

    /// Returns true if the console socket is successfully connected.
    pub fn is_connected(&self) -> bool {
        self.fd().read().map(|fd| fd.is_some()).unwrap_or_else(|e| {
            error!("Unable to retrieve fd lock: {}", e);
            false
        })
    }

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

    fn listen(path: &Path, fd_state: Arc<RwLock<Option<RawFd>>>) -> Result<()> {
        Runtime::new()?.block_on(async move {
            let listener = UnixListener::bind(&path)?;
            debug!("Listening console socket on {}", &path.display());

            // Update the permissions
            let mut perms = fs::metadata(&path).await?.permissions();
            perms.set_mode(0o700);
            fs::set_permissions(&path, perms).await?;

            let stream = listener.accept().await?.0;
            debug!("Got console socket stream: {:?}", stream);

            Self::handle_fd_receive(stream, path, fd_state).await
        })
    }

    async fn handle_fd_receive(
        mut stream: UnixStream,
        path: &Path,
        fd_state: Arc<RwLock<Option<RawFd>>>,
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

                    debug!("Shutting down stream");
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

                    debug!("Setting internal file descriptor state");
                    *fd_state
                        .write()
                        .map_err(|e| format_err!("locking fd state: {}", e))? = Some(fd);

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
    use sendfd::SendWithFd;
    use std::{os::unix::io::AsRawFd, time::Duration};
    use tokio::fs::File;

    fn wait_for<F: Fn() -> bool>(x: F) -> Result<()> {
        for _ in 1..100 {
            if x() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
        bail!("failed to wait for condition")
    }

    #[tokio::test]
    async fn new_success() -> Result<()> {
        let sut = Console::new()?;
        wait_for(|| sut.path().exists())?;

        let file = File::open("/dev/pts/ptmx").await?;
        let fd = file.as_raw_fd();

        let stream = UnixStream::connect(sut.path()).await?;
        loop {
            let ready = stream.ready(Interest::WRITABLE).await?;
            if ready.is_writable() {
                match stream.send_with_fd(b"test", &[fd]) {
                    Ok(_) => break,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Err(e) => bail!(e),
                }
            }
        }

        wait_for(|| !sut.path().exists())?;
        wait_for(|| sut.is_connected())?;

        Ok(())
    }
}
