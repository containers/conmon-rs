//! Pseudo terminal implementation.

use crate::{
    container_io::Message,
    container_log::{Pipe, SharedContainerLog},
};
use anyhow::{Context, Result};
use getset::{Getters, MutGetters};
use log::{debug, error};
use nix::errno::Errno;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use tokio::{
    fs::File,
    io::{AsyncReadExt, BufReader},
    process::{ChildStderr, ChildStdout},
    sync::mpsc,
    task,
};

#[derive(Debug, Getters, MutGetters)]
#[getset(get)]
pub struct Streams {
    #[getset(get = "pub")]
    logger: SharedContainerLog,

    #[getset(get = "pub")]
    pub message_rx_stdout: mpsc::UnboundedReceiver<Message>,

    #[getset(get = "pub")]
    message_tx_stdout: mpsc::UnboundedSender<Message>,

    #[getset(get = "pub")]
    pub message_rx_stderr: mpsc::UnboundedReceiver<Message>,

    #[getset(get = "pub")]
    message_tx_stderr: mpsc::UnboundedSender<Message>,
}

impl Streams {
    /// Create a new Streams instance.
    pub fn new(logger: SharedContainerLog) -> Result<Self> {
        debug!("Creating new IO streams");

        let (message_tx_stdout, message_rx_stdout) = mpsc::unbounded_channel();
        let (message_tx_stderr, message_rx_stderr) = mpsc::unbounded_channel();

        Ok(Self {
            logger,
            message_rx_stdout,
            message_tx_stdout,
            message_rx_stderr,
            message_tx_stderr,
        })
    }

    pub fn handle_stdio_receive(&self, stdout: Option<ChildStdout>, stderr: Option<ChildStderr>) {
        debug!("Start reading from IO streams");
        let logger = self.logger().clone();
        let message_tx = self.message_tx_stdout().clone();

        if let Some(stdout) = stdout {
            task::spawn(async move {
                Self::read_loop(stdout.as_raw_fd(), Pipe::StdOut, logger, message_tx).await
            });
        }

        let logger = self.logger().clone();
        let message_tx = self.message_tx_stderr().clone();
        if let Some(stderr) = stderr {
            task::spawn(async move {
                Self::read_loop(stderr.as_raw_fd(), Pipe::StdErr, logger, message_tx).await
            });
        }
    }

    pub async fn read_loop(
        fd: RawFd,
        pipe: Pipe,
        logger: SharedContainerLog,
        message_tx: mpsc::UnboundedSender<Message>,
    ) -> Result<()> {
        let stream = unsafe { File::from_raw_fd(fd) };
        let mut reader = BufReader::new(stream);
        let mut buf = vec![0; 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    debug!("Read {} bytes", n);
                    let data = &buf[..n];

                    let mut locked_logger = logger.write().await;
                    locked_logger
                        .write(pipe, data)
                        .await
                        .context("write to log file")?;

                    message_tx
                        .send(Message::Data(data.into()))
                        .context("send data message")?;
                }
                Ok(n) if n == 0 => {
                    debug!("No more to read");

                    message_tx
                        .send(Message::Done)
                        .context("send done message")?;
                    return Ok(());
                }
                Err(e) => match Errno::from_i32(e.raw_os_error().context("get OS error")?) {
                    Errno::EIO => {
                        debug!("Stopping read loop");

                        message_tx
                            .send(Message::Done)
                            .context("send done message")?;
                        return Ok(());
                    }
                    Errno::EBADF => {
                        return Err(Errno::EBADFD.into());
                    }
                    Errno::EAGAIN => {
                        continue;
                    }
                    _ => error!(
                        "Unable to read from file descriptor: {} {}",
                        e,
                        e.raw_os_error().context("get OS error")?
                    ),
                },
                _ => {}
            }
        }
    }
}
