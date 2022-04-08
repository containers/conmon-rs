//! Pseudo terminal implementation.

use crate::{
    container_io::Message,
    container_log::{Pipe, SharedContainerLog},
};
use anyhow::{format_err, Context, Result};
use getset::Getters;
use log::{debug, error, trace};
use std::{
    os::unix::io::{FromRawFd, AsRawFd, RawFd},
    sync::mpsc::{self, Receiver, Sender},
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, BufReader},
    process::{ChildStdout, ChildStderr},
    select,
    sync::{broadcast, oneshot},
    task,
};

#[derive(Debug, Getters)]
#[getset(get)]
pub struct Streams {
    #[getset(get = "pub")]
    logger: SharedContainerLog,

    #[getset(get = "pub")]
    message_rx: Receiver<Message>,

    #[getset(get = "pub")]
    message_tx: Sender<Message>,

    #[getset(get = "pub")]
    stop_tx: broadcast::Sender<()>,

    #[getset(get = "pub")]
    stop_rx: broadcast::Receiver<()>,
}

impl Streams {
    /// Create a new Streams instance.
    pub fn new(logger: SharedContainerLog) -> Result<Self> {
        debug!("Creating new IO streams");

        let (message_tx, message_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = broadcast::channel(1);

        Ok(Self {
            logger,
            message_rx,
            message_tx,
            stop_tx,
            stop_rx,
        })
    }

    pub fn handle_stdio_receive(&self, stdout: Option<ChildStdout>, stderr: Option<ChildStderr>) {
        debug!("Start reading from IO streams");
        let logger = self.logger().clone();
        let message_tx = self.message_tx().clone();
        let stop_rx = self.stop_tx().subscribe();

        if let Some(stdout) = stdout {
            task::spawn(async move {
                Self::read_loop_single_stream(
                    logger,
                    Pipe::StdOut,
                    message_tx,
                    stop_rx,
                    stdout.as_raw_fd(),
                )
                .await
            });
        }

        let logger = self.logger().clone();
        let message_tx = self.message_tx().clone();
        let stop_rx = self.stop_tx().subscribe();
        if let Some(stderr) = stderr {
            task::spawn(async move {
                Self::read_loop_single_stream(
                    logger,
                    Pipe::StdErr,
                    message_tx,
                    stop_rx,
                    stderr.as_raw_fd(),
                )
                .await
            });
        }
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
        let mut reader = BufReader::with_capacity(1024, stream);
        let mut buf = vec![0; 1024];
        loop {
            select! {
                res = reader.read(&mut buf) => match res {
                    Ok(n) if n > 0 => {
                        debug!("Read {} bytes", n);
                        let data = &buf[..n];
                        debug!("got data {:?}", std::str::from_utf8(data));

                        let mut locked_logger = logger.write().await;
                        locked_logger
                            .write(pipe, data)
                            .await
                            .context("write to log file")?;

                        if let Err(e) = message_tx.send(Message::Data(data.into())) {
                            debug!("Unable to send data through message channel: {}", e);
                        }
                    }
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            continue;
                        }
                        error!("Unable to read from io fd: {}", e);
                    }
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
