use crate::{
    attach::SharedContainerAttach, container_log::SharedContainerLog, streams::Streams,
    terminal::Terminal,
};
use anyhow::{Context, Result};
use log::{debug, error};
use nix::errno::Errno;
use std::os::unix::io::{FromRawFd, RawFd};
use strum::AsRefStr;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    time,
};

/// A generic abstraction over various container input-output types
pub enum ContainerIO {
    Terminal(Terminal),
    Streams(Streams),
}

/// A message to be sent through the ContainerIO.
#[derive(Debug)]
pub enum Message {
    Data(Vec<u8>),
    Done,
}

#[derive(AsRefStr, Clone, Copy, Debug)]
#[strum(serialize_all = "lowercase")]
/// Available pipe types.
pub enum Pipe {
    /// Standard output.
    StdOut,

    /// Standard error.
    StdErr,
}

impl From<Terminal> for ContainerIO {
    fn from(c: Terminal) -> Self {
        Self::Terminal(c)
    }
}

impl From<Streams> for ContainerIO {
    fn from(i: Streams) -> Self {
        Self::Streams(i)
    }
}

impl ContainerIO {
    /// Create a new container IO instance.
    pub fn new(
        terminal: bool,
        logger: SharedContainerLog,
        attach: SharedContainerAttach,
    ) -> Result<Self> {
        Ok(if terminal {
            Terminal::new(logger, attach)
                .context("create new terminal")?
                .into()
        } else {
            Streams::new(logger, attach)
                .context("create new streams")?
                .into()
        })
    }

    pub async fn read_all_with_timeout(
        &mut self,
        time_to_timeout: Option<tokio::time::Instant>,
    ) -> (Vec<u8>, Vec<u8>, bool) {
        match self {
            ContainerIO::Terminal(t) => {
                let (stdout, timed_out) =
                    Self::read_stream_with_timeout(time_to_timeout, t.message_rx_mut()).await;
                (stdout, vec![], timed_out)
            }
            ContainerIO::Streams(s) => {
                let stdout_rx = &mut s.message_rx_stdout;
                let stderr_rx = &mut s.message_rx_stderr;
                let (stdout, stderr) = tokio::join!(
                    Self::read_stream_with_timeout(time_to_timeout, stdout_rx),
                    Self::read_stream_with_timeout(time_to_timeout, stderr_rx),
                );
                let timed_out = stdout.1 || stderr.1;
                (stdout.0, stderr.0, timed_out)
            }
        }
    }

    async fn read_stream_with_timeout(
        time_to_timeout: Option<time::Instant>,
        receiver: &mut UnboundedReceiver<Message>,
    ) -> (Vec<u8>, bool) {
        let mut stdio = vec![];
        let mut timed_out = false;
        loop {
            let msg = if let Some(time_to_timeout) = time_to_timeout {
                match time::timeout_at(time_to_timeout, receiver.recv()).await {
                    Ok(Some(msg)) => msg,
                    Err(_) => {
                        timed_out = true;
                        Message::Done
                    }
                    Ok(None) => unreachable!(),
                }
            } else {
                match receiver.recv().await {
                    Some(msg) => msg,
                    None => Message::Done,
                }
            };

            match msg {
                Message::Data(s) => stdio.extend(s),
                Message::Done => break,
            }
        }
        (stdio, timed_out)
    }

    pub async fn read_loop(
        fd: RawFd,
        pipe: Pipe,
        logger: SharedContainerLog,
        message_tx: UnboundedSender<Message>,
        attach: SharedContainerAttach,
    ) -> Result<()> {
        let stream = unsafe { File::from_raw_fd(fd) };
        let mut reader = BufReader::new(stream);
        let mut buf = vec![0; 1024];

        loop {
            match reader.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    debug!("fd:{}:read {} bytes", fd, n);
                    let data = &buf[..n];

                    let mut locked_logger = logger.write().await;
                    locked_logger
                        .write(pipe, data)
                        .await
                        .context("write to log file")?;

                    attach
                        .write(pipe, data)
                        .await
                        .context("write to attach endpoints")?;

                    message_tx
                        .send(Message::Data(data.into()))
                        .context("send data message")?;
                }
                Ok(n) if n == 0 => {
                    debug!("fd:{}:No more to read", fd);

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

    pub async fn read_loop_stdin(fd: RawFd, attach: SharedContainerAttach) -> Result<()> {
        let mut writer = unsafe { File::from_raw_fd(fd) };
        loop {
            if let Some(data) = attach
                .try_read()
                .await
                .context("read from stdin attach endpoints")?
            {
                writer
                    .write_all(&data)
                    .await
                    .context("write attach stdin to stream")?;
            }
        }
    }
}
