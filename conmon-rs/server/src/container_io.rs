use crate::{
    attach::SharedContainerAttach, container_log::SharedContainerLog, streams::Streams,
    terminal::Terminal,
};
use anyhow::{bail, Context, Result};
use getset::{Getters, MutGetters};
use nix::errno::Errno;
use std::{
    fmt,
    marker::Unpin,
    os::unix::io::{FromRawFd, RawFd},
    path::{Path, PathBuf},
    sync::Arc,
};
use strum::AsRefStr;
use tempfile::Builder;
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    select,
    sync::{
        mpsc::{UnboundedReceiver, UnboundedSender},
        RwLock,
    },
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

/// A shared container IO abstraction.
#[derive(Debug, Clone)]
pub struct SharedContainerIO(Arc<RwLock<ContainerIO>>);

impl SharedContainerIO {
    /// Create a new SharedContainerIO instance from the provided ContainerIO.
    pub fn new(io: ContainerIO) -> Self {
        Self(Arc::new(RwLock::new(io)))
    }

    pub async fn read_all_with_timeout(
        &self,
        timeout: Option<Instant>,
    ) -> Result<(Vec<u8>, Vec<u8>, bool)> {
        self.0.write().await.read_all_with_timeout(timeout).await
    }

    /// Resize the shared container IO to the provided with and height.
    /// Errors in case of no terminal containers.
    pub async fn resize(&self, width: u16, height: u16) -> Result<()> {
        match self.0.read().await.typ() {
            ContainerIOType::Terminal(t) => t.resize(width, height).context("resize terminal"),
            ContainerIOType::Streams(_) => bail!("container has no terminal"),
        }
    }

    /// Retrieve the underlying SharedContainerLog instance.
    pub async fn logger(&self) -> SharedContainerLog {
        self.0.read().await.logger().clone()
    }

    /// Retrieve the underlying SharedContainerAttach instance.
    pub async fn attach(&self) -> SharedContainerAttach {
        self.0.read().await.attach().clone()
    }
}

#[derive(Debug, Getters, MutGetters)]
pub struct ContainerIO {
    #[getset(get = "pub", get_mut = "pub")]
    typ: ContainerIOType,

    #[getset(get = "pub")]
    logger: SharedContainerLog,

    #[getset(get = "pub")]
    attach: SharedContainerAttach,
}

#[derive(Debug)]
/// A generic abstraction over various container input-output types
pub enum ContainerIOType {
    Terminal(Terminal),
    Streams(Streams),
}

/// A message to be sent through the ContainerIO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Data(Vec<u8>, Pipe),
    Done,
}

#[derive(AsRefStr, Clone, Copy, Debug, PartialEq, Eq)]
#[strum(serialize_all = "lowercase")]
/// Available pipe types.
pub enum Pipe {
    /// Standard output.
    StdOut,

    /// Standard error.
    StdErr,
}

impl fmt::Display for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}

impl From<Terminal> for ContainerIOType {
    fn from(c: Terminal) -> Self {
        Self::Terminal(c)
    }
}

impl From<Streams> for ContainerIOType {
    fn from(i: Streams) -> Self {
        Self::Streams(i)
    }
}

impl ContainerIO {
    const MAX_STDIO_STREAM_SIZE: usize = 16 * 1024 * 1024;

    /// Create a new container IO instance.
    pub fn new(terminal: bool, logger: SharedContainerLog) -> Result<Self> {
        let logger_clone = logger.clone();
        let attach = SharedContainerAttach::default();
        let attach_clone = attach.clone();
        let typ = if terminal {
            Terminal::new(logger_clone, attach_clone)
                .context("create new terminal")?
                .into()
        } else {
            Streams::new(logger_clone, attach_clone)
                .context("create new streams")?
                .into()
        };
        Ok(Self {
            typ,
            logger,
            attach,
        })
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

    pub async fn read_all_with_timeout(
        &mut self,
        time_to_timeout: Option<Instant>,
    ) -> Result<(Vec<u8>, Vec<u8>, bool)> {
        match self.typ_mut() {
            ContainerIOType::Terminal(t) => {
                if let Some(message_rx) = t.message_rx_mut() {
                    let (stdout, timed_out) =
                        Self::read_stream_with_timeout(time_to_timeout, message_rx).await;
                    Ok((stdout, vec![], timed_out))
                } else {
                    bail!("read_all_with_timeout called before message_rx was registered");
                }
            }
            ContainerIOType::Streams(s) => {
                let stdout_rx = &mut s.message_rx_stdout;
                let stderr_rx = &mut s.message_rx_stderr;
                let (stdout, stderr) = tokio::join!(
                    Self::read_stream_with_timeout(time_to_timeout, stdout_rx),
                    Self::read_stream_with_timeout(time_to_timeout, stderr_rx),
                );
                let timed_out = stdout.1 || stderr.1;
                Ok((stdout.0, stderr.0, timed_out))
            }
        }
    }

    async fn read_stream_with_timeout(
        time_to_timeout: Option<Instant>,
        receiver: &mut UnboundedReceiver<Message>,
    ) -> (Vec<u8>, bool) {
        let mut stdio = vec![];
        let mut timed_out = false;
        loop {
            let msg = if let Some(time_to_timeout) = time_to_timeout {
                {
                    match time::timeout_at(time_to_timeout, receiver.recv()).await {
                        Ok(Some(msg)) => msg,
                        Err(_) => {
                            timed_out = true;
                            Message::Done
                        }
                        Ok(None) => unreachable!(),
                    }
                }
            } else {
                {
                    match receiver.recv().await {
                        Some(msg) => msg,
                        None => Message::Done,
                    }
                }
            };
            match msg {
                Message::Data(data, _) => {
                    if let Some(future_len) = stdio.len().checked_add(data.len()) {
                        if future_len < Self::MAX_STDIO_STREAM_SIZE {
                            stdio.extend(data)
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                Message::Done => break,
            }
        }
        (stdio, timed_out)
    }

    pub async fn read_loop<T>(
        mut reader: T,
        pipe: Pipe,
        logger: SharedContainerLog,
        message_tx: UnboundedSender<Message>,
        mut attach: SharedContainerAttach,
        token: CancellationToken,
    ) -> Result<()>
    where
        T: AsyncRead + Unpin,
    {
        let mut buf = vec![0; 64];

        loop {
            // In situations we're processing output directly from the I/O streams
            // we need a mechanism to figure out when to stop that doesn't involve reading the
            // number of bytes read.
            // Thus, we need to select on the cancellation token saved in the child.
            // While this could result in a data race, as select statements are racy,
            // we won't interleve these two futures, as one ends execution.
            select! {
                n = reader.read(&mut buf) => {
                    match n {
                        Ok(n) if n > 0 => {
                            debug!("Read {} bytes", n);
                            let data = &buf[..n];

                            let mut locked_logger = logger.write().await;
                            locked_logger
                                .write(pipe, data)
                                .await
                                .context("write to log file")?;

                            attach
                                .write(Message::Data(data.into(), pipe))
                                .await
                                .context("write to attach endpoints")?;

                            if !message_tx.is_closed() {
                                message_tx
                                    .send(Message::Data(data.into(), pipe))
                                    .context("send data message")?;
                            }
                        }
                        Err(e) => match Errno::from_i32(e.raw_os_error().context("get OS error")?) {
                            Errno::EIO => {
                                debug!("Stopping read loop");
                                attach
                                    .write(Message::Done)
                                    .await
                                    .context("write to attach endpoints")?;

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
                _ = token.cancelled() => {
                    debug!("Sending done because token cancelled");
                    attach
                        .write(Message::Done)
                        .await
                        .context("write to attach endpoints")?;

                    message_tx
                        .send(Message::Done)
                        .context("send done message")?;
                    return Ok(());
                }
            }
        }
    }

    pub async fn read_loop_stdin(
        fd: RawFd,
        mut attach: SharedContainerAttach,
        token: CancellationToken,
    ) -> Result<()> {
        let mut writer = unsafe { File::from_raw_fd(fd) };
        loop {
            // While we're not processing input from a caller, and logically should be able to
            // catch a Message::Done here, it doesn't quite work that way.
            // Every child has an io instance that starts this function, though not
            // all children have input to process. If there is no input from the child
            // then we leak this function, causing memory to balloon over time.
            // Thus, we need the select statement and token.
            select! {
                res = attach.read() => {
                    match res {
                        Ok(data) => {
                            writer
                                .write_all(&data)
                                .await
                                .context("write attach stdin to stream")?;
                        }
                        Err(e) => {
                            return Err(e).context("read from stdin attach endpoints");
                        }
                    }
                }
                _ = token.cancelled() => {
                    debug!("Exiting because token cancelled");
                    return Ok(());
                }
            }
        }
    }
}
