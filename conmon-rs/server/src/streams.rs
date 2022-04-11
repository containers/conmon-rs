//! Pseudo terminal implementation.

use crate::{
    container_io::Message,
    container_log::{Pipe, SharedContainerLog},
};
use anyhow::{format_err, Context, Result};
use getset::Getters;
use log::{debug, error, trace};
use nix::{
    fcntl::OFlag,
    sys::stat::{self, Mode},
    unistd,
};
use std::{
    fs::OpenOptions,
    os::unix::io::{FromRawFd, IntoRawFd, RawFd},
    str,
    sync::mpsc::{self, Receiver, Sender},
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, BufReader},
    select,
    sync::{broadcast, oneshot},
    task,
};

#[derive(Debug, Getters)]
#[getset(get)]
pub struct Streams {
    #[getset(get = "pub")]
    message_rx: Receiver<Message>,

    #[getset(get = "pub")]
    stop_tx: broadcast::Sender<()>,
}

impl Streams {
    /// Create a new Streams instance.
    pub fn new(logger: SharedContainerLog) -> Result<Self> {
        debug!("Creating new IO streams");
        Self::disconnect_std_streams().context("disconnect standard streams")?;

        let (stdout_fd_read, stdout_fd_write) =
            unistd::pipe2(OFlag::O_CLOEXEC).context("create stdout pipe")?;
        unistd::dup2(stdout_fd_write, libc::STDOUT_FILENO).context("dup over stdout")?;

        let (stderr_fd_read, stderr_fd_write) =
            unistd::pipe2(OFlag::O_CLOEXEC).context("create stderr pipe")?;
        unistd::dup2(stderr_fd_write, libc::STDERR_FILENO).context("dup over stderr")?;

        let mode = Mode::from_bits_truncate(0o777);
        stat::fchmod(libc::STDOUT_FILENO, mode).context("chmod stdout")?;
        stat::fchmod(libc::STDERR_FILENO, mode).context("chmod stderr")?;

        let (message_tx, message_rx) = mpsc::channel();
        let (stop_tx, stop_rx_stdout) = broadcast::channel(1);
        let stop_rx_stderr = stop_tx.subscribe();

        task::spawn(async move {
            Self::read_loop(
                logger,
                message_tx,
                stop_rx_stdout,
                stop_rx_stderr,
                stdout_fd_read,
                stderr_fd_read,
            )
        });

        Ok(Self {
            message_rx,
            stop_tx,
        })
    }

    fn disconnect_std_streams() -> Result<()> {
        const DEV_NULL: &str = "/dev/null";

        let dev_null_r = OpenOptions::new().read(true).open(DEV_NULL)?.into_raw_fd();
        let dev_null_w = OpenOptions::new().write(true).open(DEV_NULL)?.into_raw_fd();

        unistd::dup2(dev_null_r, libc::STDIN_FILENO).context("dup over stdin")?;
        unistd::dup2(dev_null_w, libc::STDOUT_FILENO).context("dup over stdout")?;
        unistd::dup2(dev_null_w, libc::STDERR_FILENO).context("dup over stderr")?;

        Ok(())
    }

    fn read_loop(
        logger: SharedContainerLog,
        message_tx: Sender<Message>,
        stop_rx_stdout: broadcast::Receiver<()>,
        stop_rx_stderr: broadcast::Receiver<()>,
        stdout: RawFd,
        stderr: RawFd,
    ) {
        debug!("Start reading from IO streams");

        let message_tx_stdout = message_tx.clone();
        let logger_clone = logger.clone();

        task::spawn(async move {
            Self::read_loop_single_stream(
                logger,
                Pipe::StdOut,
                message_tx_stdout,
                stop_rx_stdout,
                stdout,
            )
            .await
        });
        task::spawn(async move {
            Self::read_loop_single_stream(
                logger_clone,
                Pipe::StdErr,
                message_tx,
                stop_rx_stderr,
                stderr,
            )
            .await
        });
    }

    async fn read_loop_single_stream(
        logger: SharedContainerLog,
        pipe: Pipe,
        message_tx: Sender<Message>,
        mut stop_rx: broadcast::Receiver<()>,
        fd: RawFd,
    ) -> Result<()> {
        trace!("Start reading from single fd: {:?}", fd);

        let message_tx_clone = message_tx.clone();
        let (thread_shutdown_tx, thread_shutdown_rx) = oneshot::channel();
        task::spawn(async move {
            Self::run_buffer_loop(logger, pipe, message_tx_clone, fd, thread_shutdown_rx).await
        });

        stop_rx
            .recv()
            .await
            .context("unable to wait for stop channel")?;
        debug!("Received IO stream stop signal");
        thread_shutdown_tx
            .send(())
            .map_err(|_| format_err!("unable to send thread shutdown message"))?;
        message_tx.send(Message::Done).context("send done message")
    }

    async fn run_buffer_loop(
        logger: SharedContainerLog,
        pipe: Pipe,
        message_tx: Sender<Message>,
        fd: RawFd,
        mut thread_shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<()> {
        let stream = unsafe { File::from_raw_fd(fd) };
        let mut reader = BufReader::new(stream);
        let mut buf = vec![0; 1024];
        loop {
            select! {
                res = reader.read(&mut buf) => match res {
                    Ok(n) if n > 0 => {
                        debug!("Read {} bytes", n);
                        let data = &buf[..n];

                        let mut locked_logger = logger.write().await;
                        locked_logger
                            .write(pipe, data)
                            .await
                            .context("write to log file")?;

                        if let Err(e) = message_tx.send(Message::Data(data.into())) {
                            error!("Unable to send data through message channel: {}", e);
                        }
                    }
                    Err(e) => error!("Unable to read from io fd: {}", e),
                    _ => {}
                },
                _ = &mut thread_shutdown_rx => {
                    debug!("Shutting down io streams thread");
                    return Ok(())
                }
            }
        }
    }
}
