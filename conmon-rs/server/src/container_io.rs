use crate::{
    attach::SharedContainerAttach, container_log::SharedContainerLog, streams::Streams,
    terminal::Terminal,
};
use anyhow::{Context, Result, bail};
use async_channel::{Receiver, Sender};
use getset::{Getters, MutGetters};
use nix::errno::Errno;
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};
use strum::AsRefStr;
use tempfile::Builder;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
    sync::RwLock,
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace};

/// Buffer size for reading container I/O streams
const READ_BUFFER_SIZE: usize = 4096;

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

    /// Retrieve the underlying stdout and stderr channels.
    pub async fn stdio(&self) -> Result<(Receiver<Message>, Receiver<Message>)> {
        self.0.read().await.stdio()
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

    /// Retrieve clones of the stdout and stderr channels.
    pub fn stdio(&self) -> Result<(Receiver<Message>, Receiver<Message>)> {
        match self.typ() {
            ContainerIOType::Terminal(t) => {
                if let Some(message_rx) = t.message_rx() {
                    let (_, fake_rx) = async_channel::bounded(10);
                    Ok((message_rx.clone(), fake_rx))
                } else {
                    bail!("called before message receiver was registered")
                }
            }
            ContainerIOType::Streams(s) => {
                Ok((s.message_rx_stdout.clone(), s.message_rx_stderr.clone()))
            }
        }
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
        receiver: &mut Receiver<Message>,
    ) -> (Vec<u8>, bool) {
        // Pre-allocate with reasonable capacity to avoid reallocations
        // Start with 64KB which handles most exec_sync outputs without reallocation
        let mut stdio = Vec::with_capacity(64 * 1024);
        let mut timed_out = false;
        loop {
            let msg = if let Some(time_to_timeout) = time_to_timeout {
                {
                    match time::timeout_at(time_to_timeout, receiver.recv()).await {
                        Ok(Ok(msg)) => msg,
                        _ => {
                            timed_out = true;
                            Message::Done
                        }
                    }
                }
            } else {
                {
                    match receiver.recv().await {
                        Ok(msg) => msg,
                        _ => Message::Done,
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
        message_tx: Sender<Message>,
        mut attach: SharedContainerAttach,
    ) -> Result<()>
    where
        T: AsyncRead + Unpin,
    {
        let mut buf = vec![0; READ_BUFFER_SIZE];

        loop {
            match reader.read(&mut buf).await {
                Ok(0) => {
                    debug!("Nothing more to read");

                    attach
                        .write(Message::Done)
                        .await
                        .context("write to attach endpoints")?;

                    message_tx
                        .force_send(Message::Done)
                        .context("send done message")?;

                    return Ok(());
                }

                Ok(n) => {
                    trace!("Read {} bytes", n);
                    let data = &buf[..n];

                    let mut locked_logger = logger.write().await;
                    locked_logger
                        .write(pipe, data)
                        .await
                        .context("write to log file")?;

                    // Convert to Vec once and clone for second use to avoid double allocation
                    let data_vec: Vec<u8> = data.into();
                    attach
                        .write(Message::Data(data_vec.clone(), pipe))
                        .await
                        .context("write to attach endpoints")?;

                    message_tx
                        .force_send(Message::Data(data_vec, pipe))
                        .context("send data message")?;
                }

                Err(e) => match Errno::from_raw(e.raw_os_error().context("get OS error")?) {
                    Errno::EIO => {
                        debug!("Stopping read loop");
                        attach
                            .write(Message::Done)
                            .await
                            .context("write to attach endpoints")?;

                        message_tx
                            .force_send(Message::Done)
                            .context("send done message")?;

                        return Ok(());
                    }
                    Errno::EBADF => bail!(e),
                    Errno::EAGAIN => continue,
                    _ => error!(
                        "Unable to read from file descriptor: {} {}",
                        e,
                        e.raw_os_error().context("get OS error")?
                    ),
                },
            }
        }
    }

    pub async fn read_loop_stdin(
        mut writer: impl AsyncWrite + Unpin,
        mut attach: SharedContainerAttach,
        token: CancellationToken,
    ) -> Result<()> {
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
                            Self::handle_stdin_data(&data, &mut writer).await?;
                        }
                        Err(e) => {
                            return Err(e).context("read from stdin attach endpoints");
                        }
                    }
                }
                _ = token.cancelled() => {
                    // Closing immediately may race with outstanding data on stdin for short lived
                    // containers. This means we try to read once again.
                    if let Ok(data) = attach.try_read() {
                        Self::handle_stdin_data(&data, &mut writer).await?;
                    }
                    return Ok(());
                }
            }
        }
    }

    async fn handle_stdin_data(data: &[u8], mut writer: impl AsyncWrite + Unpin) -> Result<()> {
        debug!("Got {} attach bytes", data.len());

        writer
            .write_all(data)
            .await
            .context("write attach stdin to stream")
    }
}
