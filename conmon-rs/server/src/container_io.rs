use crate::{
    attach::{AttachOutput, SharedContainerAttach},
    child::sleep_until,
    container_log::SharedContainerLog,
    random_id::RandomId,
    streams::Streams,
    tail::Tail,
    terminal::Terminal,
};
use anyhow::{Context, Result, bail};
use async_channel::{Receiver, RecvError, Sender};
use bytes::{Bytes, BytesMut};
use nix::errno::Errno;
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
    sync::{RwLock, broadcast},
    task::{self, JoinHandle},
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

/// How many read chunks the stdio and terminal channels buffer per pipe.
pub const STDIO_CHANNEL_SIZE: usize = 100;

/// Buffer size for reading container I/O streams. Every read loop keeps one
/// buffer of this size, so an idle container holds it once per pipe. 8 KiB
/// halves the reads and log writes of 4 KiB for bulk output and keeps what a
/// container holds after it in line with the log buffers, see
/// `LogFile::FLUSH_SIZE`. Small reads get copied out of it and do not pin
/// it, see [`ContainerIO::read_pipe`].
pub(crate) const READ_BUFFER_SIZE: usize = 8 * 1024;

/// Upper bound for waiting on the next output after the process exited. The
/// read loops may still be draining the pipes when the exit is observed, they
/// signal the end with `Message::Done`. The timeout only kicks in if that
/// never arrives, for example because a grandchild keeps the pipe open.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

/// Upper bound for the messages forwarded per drain after the process exited.
/// The output written before the exit is at most what the channels (100
/// messages) and the pipe buffers hold. With the default pipe-max-size of
/// 1 MiB in 8 KiB reads, that is 128 messages per pipe, and small writes
/// read one by one do not fill a pipe of that size either, so this is well
/// above. It is a heuristic though, a process with CAP_SYS_RESOURCE can grow
/// its pipes further. More output stems from a grandchild which keeps the
/// pipe open and writing, which would otherwise keep the drain going forever.
pub(crate) const DRAIN_MAX_MESSAGES: usize = 1024;

/// Upper bound for the time a drain waits for output after the process
/// exited in total. A grandchild which keeps writing just often enough for
/// [`DRAIN_TIMEOUT`] would keep it going for minutes otherwise. Only the
/// waiting counts, not the forwarding, so a slow client still gets the
/// output which was there already.
pub(crate) const DRAIN_MAX_WAIT: Duration = Duration::from_secs(1);

/// Bounds forwarding the output after the process exited.
pub(crate) struct Drain {
    remaining: usize,
    waited: Duration,
}

impl Default for Drain {
    fn default() -> Self {
        Self {
            remaining: DRAIN_MAX_MESSAGES,
            waited: Duration::ZERO,
        }
    }
}

impl Drain {
    /// Wait for the next message, giving up once the channel stays quiet for
    /// [`DRAIN_TIMEOUT`], [`DRAIN_MAX_MESSAGES`] were received or the waits
    /// took [`DRAIN_MAX_WAIT`].
    pub(crate) async fn recv<F: Future>(&mut self, recv: F) -> Option<F::Output> {
        let Some(remaining) = self.remaining.checked_sub(1) else {
            warn!(
                "Dropping remaining output after forwarding {DRAIN_MAX_MESSAGES} messages since the exit"
            );
            return None;
        };
        self.remaining = remaining;
        let budget = DRAIN_MAX_WAIT.saturating_sub(self.waited);
        if budget.is_zero() {
            debug!("Dropping remaining output after waiting {DRAIN_MAX_WAIT:?} since the exit");
            return None;
        }
        let start = Instant::now();
        let res = time::timeout(DRAIN_TIMEOUT.min(budget), recv).await.ok();
        self.waited += start.elapsed();
        res
    }

    /// The next message of the attach output with [`Drain::recv`]. A
    /// receiver which fell behind skips messages, which gets logged, as a
    /// warning for the receiver of a client. None once the output ended or
    /// the drain gave up.
    pub(crate) async fn recv_output(
        &mut self,
        rx: &mut broadcast::Receiver<Message>,
        client: bool,
    ) -> Option<Message> {
        loop {
            match self.recv(rx.recv()).await? {
                Ok(Message::Done) | Err(broadcast::error::RecvError::Closed) => return None,
                Ok(message) => return Some(message),
                Err(broadcast::error::RecvError::Lagged(n)) if client => {
                    warn!("Attach client too slow, skipped {n} messages")
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("Skipped {n} attach output messages")
                }
            }
        }
    }
}

/// The sending half of a stdio channel, which a read loop forwards the
/// output of its pipe to.
///
/// Until a consumer attached, see [`ContainerIO::consume_stdio`], the channel
/// keeps the latest chunks only and drops the oldest one when it is full. The
/// output of a container must never wait for a consumer which may not exist,
/// only the stderr of a failed runtime is of interest. Once one attached, a
/// full channel makes the read loop wait, so that a slow consumer gets all
/// output and the process waits for it instead. An exec gets its consumer
/// before its runtime starts, see [`ContainerIO::collect_stdio`]. The
/// consumer closes the channel when it is done, the read loop drops the
/// sender then and keeps reading the pipe.
#[derive(Debug)]
pub struct StdioSender {
    tx: Sender<Message>,
    consumed: Arc<AtomicBool>,
}

impl StdioSender {
    pub fn new(tx: Sender<Message>, consumed: &Arc<AtomicBool>) -> Self {
        Self {
            tx,
            consumed: consumed.clone(),
        }
    }

    /// Send a message, false if the channel got closed or all receivers are
    /// gone.
    async fn send(&self, message: Message) -> bool {
        if self.consumed.load(Ordering::Acquire) {
            self.tx.send(message).await.is_ok()
        } else {
            self.tx.force_send(message).is_ok()
        }
    }
}

/// Closes a stdio channel on drop, also if the consumer future gets dropped.
struct CloseOnDrop<'a>(&'a Receiver<Message>);

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Logs a failing log driver once until it works again. A full disk would
/// otherwise log an error for every chunk of output.
#[derive(Debug, Default)]
struct LogErrors {
    failing: bool,
}

impl LogErrors {
    fn check(&mut self, pipe: Pipe, res: Result<()>) {
        match res {
            Ok(()) if self.failing => {
                self.failing = false;
                info!("Writing the log of {pipe} works again");
            }
            Ok(()) => {}
            Err(e) if !self.failing => {
                self.failing = true;
                error!("Unable to write the log of {pipe}, dropping output until it works: {e:#}");
            }
            Err(e) => trace!("Unable to write the log of {pipe}: {e:#}"),
        }
    }
}

/// A shared container IO abstraction.
#[derive(Debug, Clone)]
pub struct SharedContainerIO(Arc<RwLock<ContainerIO>>);

impl SharedContainerIO {
    /// Create a new SharedContainerIO instance from the provided ContainerIO.
    pub fn new(io: ContainerIO) -> Self {
        Self(Arc::new(RwLock::new(io)))
    }

    /// Read the output of an exec, see [`ContainerIO::read_all_with_timeout`].
    pub async fn read_all_with_timeout(
        &self,
        timeout: Option<Instant>,
        exited: &CancellationToken,
    ) -> Result<(Vec<u8>, Vec<u8>, bool)> {
        self.0
            .write()
            .await
            .read_all_with_timeout(timeout, exited)
            .await
    }

    /// Resize the shared container IO to the provided with and height.
    /// Errors in case of no terminal containers.
    pub async fn resize(&self, width: u16, height: u16) -> Result<()> {
        if !self.try_resize(width, height).await? {
            bail!("container has no terminal")
        }
        Ok(())
    }

    /// Resize the terminal if the container has one and return whether it
    /// does.
    pub async fn try_resize(&self, width: u16, height: u16) -> Result<bool> {
        match self.0.read().await.typ() {
            ContainerIOType::Terminal(t) => t.resize(width, height).context("resize terminal")?,
            ContainerIOType::Streams(_) => return Ok(false),
        }
        Ok(true)
    }

    /// The end of file character of the terminal, none without one.
    pub async fn terminal_eof(&self) -> Option<u8> {
        match self.0.read().await.typ() {
            ContainerIOType::Terminal(t) => Some(t.eof_char()),
            ContainerIOType::Streams(_) => None,
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

/// A file path which gets removed on drop.
#[derive(Debug)]
pub struct TempFile(PathBuf);

impl TempFile {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // A plain unlink does not need the runtime and cannot panic without one.
        if let Err(e) = std::fs::remove_file(&self.0) {
            debug!("Unable to remove temp file {}: {}", self.0.display(), e);
        }
    }
}

#[derive(Debug)]
pub struct ContainerIO {
    typ: ContainerIOType,
    logger: SharedContainerLog,
    attach: SharedContainerAttach,

    /// Collects the output of an exec while its runtime runs, see
    /// [`ContainerIO::collect_stdio`].
    collector: Option<StdioCollector>,
}

/// The output which an exec wrote while its runtime ran, per pipe.
#[derive(Debug, Default)]
pub struct EarlyStdio {
    pub stdout: EarlyStream,
    pub stderr: EarlyStream,
}

/// The output of a pipe up to [`ContainerIO::MAX_STDIO_STREAM_SIZE`].
#[derive(Debug, Default)]
pub struct EarlyStream {
    pub data: Vec<u8>,

    /// Whether the output ended.
    pub done: bool,

    /// Whether output got dropped because of the size limit. Nothing may be
    /// appended then, it would leave a gap.
    pub truncated: bool,

    /// The end of the output which got dropped, where the error of a runtime
    /// which failed is, up to [`EarlyStream::TAIL_SIZE`].
    tail: Tail<{ EarlyStream::TAIL_SIZE }>,
}

impl EarlyStream {
    const TAIL_SIZE: usize = 64 * 1024;

    fn push(&mut self, data: &[u8]) {
        let len = self.data.len() + data.len();
        if self.truncated || len >= ContainerIO::MAX_STDIO_STREAM_SIZE {
            self.truncated = true;
            self.tail.push(data);
            return;
        }
        // Grow like a vector would, but not beyond the limit.
        if len > self.data.capacity() {
            let capacity = len
                .max(2 * self.data.capacity())
                .min(ContainerIO::MAX_STDIO_STREAM_SIZE);
            self.data.reserve_exact(capacity - self.data.len());
        }
        self.data.extend_from_slice(data);
    }

    /// The last [`EarlyStream::TAIL_SIZE`] bytes of the output, also of the
    /// part which got dropped. The dropped part continues the kept data, a
    /// short one gets completed by the end of the data.
    fn end(&self) -> Vec<u8> {
        let tail = if self.truncated { self.tail.end() } else { &[] };
        let from_data = Self::TAIL_SIZE.saturating_sub(tail.len());
        let data = &self.data[self.data.len().saturating_sub(from_data)..];
        [data, tail].concat()
    }
}

#[derive(Debug)]
struct StdioCollector {
    stop: CancellationToken,
    task: JoinHandle<EarlyStdio>,
}

impl Drop for StdioCollector {
    /// The collector must not outlive its exec, which may have failed before
    /// the output got taken.
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

impl ContainerIO {
    pub fn typ(&self) -> &ContainerIOType {
        &self.typ
    }
    pub fn typ_mut(&mut self) -> &mut ContainerIOType {
        &mut self.typ
    }
    pub fn logger(&self) -> &SharedContainerLog {
        &self.logger
    }
    pub fn attach(&self) -> &SharedContainerAttach {
        &self.attach
    }
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
    Data(Bytes, Pipe),
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Available pipe types.
pub enum Pipe {
    /// Standard output.
    StdOut,

    /// Standard error.
    StdErr,
}

impl Pipe {
    /// The name of the pipe as used in the logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StdOut => "stdout",
            Self::StdErr => "stderr",
        }
    }
}

impl fmt::Display for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.as_str())
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
        let attach = SharedContainerAttach::new(terminal);
        let attach_clone = attach.clone();
        let typ = if terminal {
            Terminal::new(logger_clone, attach_clone)
                .context("create new terminal")?
                .into()
        } else {
            Streams::new(logger_clone, attach_clone).into()
        };
        Ok(Self {
            typ,
            logger,
            attach,
            collector: None,
        })
    }

    /// Generate a unique temp file path without creating the file.
    fn temp_file_name(directory: Option<&Path>, prefix: &str, suffix: &str) -> PathBuf {
        let dir = directory
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let unique = RandomId::new();
        dir.join(format!("{prefix}{unique}{suffix}"))
    }

    /// A unique file path like [`ContainerIO::temp_file_name`], whose file
    /// gets removed on drop. This covers early returns and cancelled futures,
    /// which would leave for example the PID file of an exec behind.
    pub fn temp_file(directory: Option<&Path>, prefix: &str, suffix: &str) -> TempFile {
        TempFile(Self::temp_file_name(directory, prefix, suffix))
    }

    /// Retrieve clones of the stdout and stderr channels.
    ///
    /// Fails if the channels have been dropped via [`ContainerIO::drop_stdio`].
    pub fn stdio(&self) -> Result<(Receiver<Message>, Receiver<Message>)> {
        match self.typ() {
            ContainerIOType::Terminal(t) => {
                if let Some(message_rx) = t.message_rx() {
                    let (_, fake_rx) = async_channel::bounded(1);
                    Ok((message_rx.clone(), fake_rx))
                } else {
                    bail!("stdio channel not available")
                }
            }
            ContainerIOType::Streams(s) => s.stdio(),
        }
    }

    /// Drop the stdout and stderr channels.
    ///
    /// The channels buffer the last chunks per pipe for consumers like
    /// `exec_sync`, which may start reading after the process already produced
    /// output, and for reporting the runtime's stderr if a create fails. Once
    /// a container is created nothing consumes them anymore, attach sessions
    /// use the attach broadcast, so they get dropped. The read loops drop their
    /// senders on the next send, which frees the channel buffers.
    pub fn drop_stdio(&mut self) {
        match self.typ_mut() {
            ContainerIOType::Terminal(t) => {
                t.message_rx_mut().take();
            }
            ContainerIOType::Streams(s) => s.drop_stdio(),
        }
    }

    /// Read the output of an exec from now on, until it gets taken with
    /// [`ContainerIO::take_early_stdio`] once its runtime finished. The
    /// process may write a lot before, which the channels alone would drop,
    /// see [`StdioSender`]. A runtime without a terminal shares the pipes,
    /// so the reads must not wait for anything: they apply backpressure,
    /// but take everything up to [`ContainerIO::MAX_STDIO_STREAM_SIZE`] per
    /// pipe and drop what follows.
    ///
    /// The output of a pipe which is not of interest gets read and dropped.
    pub fn collect_stdio(&mut self, stdout_enabled: bool, stderr_enabled: bool) -> Result<()> {
        let (stdout, stderr) = self.stdio()?;
        self.consume_stdio();
        let stop = CancellationToken::new();
        let collect = {
            let stop = stop.clone();
            async move {
                let (stdout, stderr) = tokio::join!(
                    Self::collect(&stdout, &stop, stdout_enabled),
                    Self::collect(&stderr, &stop, stderr_enabled),
                );
                EarlyStdio { stdout, stderr }
            }
        };
        self.collector = Some(StdioCollector {
            stop,
            task: task::spawn(collect),
        });
        Ok(())
    }

    /// Collect the output of a channel until `stop`, the remaining messages
    /// stay queued for the consumer which comes next. Beyond the size limit
    /// the output gets read and dropped.
    async fn collect(
        receiver: &Receiver<Message>,
        stop: &CancellationToken,
        enabled: bool,
    ) -> EarlyStream {
        let mut stream = EarlyStream::default();
        loop {
            let message = select! {
                biased;
                () = stop.cancelled() => return stream,
                message = receiver.recv() => message,
            };
            match message {
                Ok(Message::Data(data, _)) if enabled => stream.push(&data),
                Ok(Message::Data(..)) => {}
                Ok(Message::Done) => {
                    stream.done = true;
                    return stream;
                }
                Err(RecvError) => return stream,
            }
        }
    }

    /// The output collected since [`ContainerIO::collect_stdio`], empty
    /// without that.
    pub async fn take_early_stdio(&mut self) -> EarlyStdio {
        let Some(mut collector) = self.collector.take() else {
            return EarlyStdio::default();
        };
        collector.stop.cancel();
        (&mut collector.task).await.unwrap_or_else(|e| {
            error!("Unable to collect the exec output: {e}");
            EarlyStdio::default()
        })
    }

    /// Apply backpressure to the read loops from now on, see
    /// [`StdioSender`].
    pub fn consume_stdio(&self) {
        match self.typ() {
            ContainerIOType::Terminal(t) => t.consume_stdio(),
            ContainerIOType::Streams(s) => s.consume_stdio(),
        }
    }

    /// Attach the stderr output of a runtime which failed to its error. With
    /// a terminal, the error carries it already.
    pub async fn runtime_error(&mut self, e: anyhow::Error) -> anyhow::Error {
        let stderr = match self.typ() {
            ContainerIOType::Streams(_) => {
                // The runtime exited, only its remaining output is of
                // interest. Its error is at the end, also of output which
                // exceeded the size limit.
                self.consume_stdio();
                let early = self.take_early_stdio().await;
                if early.stderr.truncated {
                    early.stderr.end()
                } else {
                    let exited = CancellationToken::new();
                    exited.cancel();
                    match self.read_all_with_early(None, &exited, early).await {
                        Ok((_, stderr, _)) => stderr,
                        Err(read_err) => {
                            debug!("Unable to read runtime output: {:#}", read_err);
                            vec![]
                        }
                    }
                }
            }
            ContainerIOType::Terminal(_) => vec![],
        };
        let stderr = String::from_utf8_lossy(&stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            e
        } else {
            anyhow::format_err!("{e:#}: {stderr}")
        }
    }

    /// Read the output until it ended, but at most until the deadline, and
    /// return it with whether the deadline passed. Once `exited` got
    /// cancelled, the output still in flight is taken as long as the
    /// [`Drain`] does. A background process which holds the pipes open must
    /// not keep this waiting. The channels get closed afterwards, so that the
    /// read loops discard what follows.
    pub async fn read_all_with_timeout(
        &mut self,
        time_to_timeout: Option<Instant>,
        exited: &CancellationToken,
    ) -> Result<(Vec<u8>, Vec<u8>, bool)> {
        self.consume_stdio();
        let early = self.take_early_stdio().await;
        self.read_all_with_early(time_to_timeout, exited, early)
            .await
    }

    /// Read the output after the one collected before, see
    /// [`ContainerIO::read_all_with_timeout`].
    async fn read_all_with_early(
        &mut self,
        time_to_timeout: Option<Instant>,
        exited: &CancellationToken,
        early: EarlyStdio,
    ) -> Result<(Vec<u8>, Vec<u8>, bool)> {
        match self.typ_mut() {
            ContainerIOType::Terminal(t) => {
                if let Some(message_rx) = t.message_rx_mut() {
                    let (stdout, timed_out) =
                        Self::read_stream(time_to_timeout, exited, message_rx, early.stdout).await;
                    Ok((stdout, vec![], timed_out))
                } else {
                    bail!("read_all_with_timeout called before message_rx was registered");
                }
            }
            ContainerIOType::Streams(s) => {
                let (stdout_rx, stderr_rx) = s.stdio_mut()?;
                let (stdout, stderr) = tokio::join!(
                    Self::read_stream(time_to_timeout, exited, stdout_rx, early.stdout),
                    Self::read_stream(time_to_timeout, exited, stderr_rx, early.stderr),
                );
                let timed_out = stdout.1 || stderr.1;
                Ok((stdout.0, stderr.0, timed_out))
            }
        }
    }

    /// Read a stream, after the `early` output collected before.
    async fn read_stream(
        time_to_timeout: Option<Instant>,
        exited: &CancellationToken,
        receiver: &Receiver<Message>,
        early: EarlyStream,
    ) -> (Vec<u8>, bool) {
        // Also after the size limit, a process which keeps writing must not
        // wait for this consumer.
        let _close = CloseOnDrop(receiver);

        // The collector kept below the limit. Without it, start with 4 KiB,
        // which handles most exec_sync outputs, doubling handles growth.
        let mut stdio = early.data;
        if early.done || early.truncated {
            return (stdio, false);
        }
        stdio.reserve(4096);
        let mut drain = None;
        loop {
            let res = select! {
                // An exit before the deadline ends the timeout, the drain is
                // bounded on its own. Output after that is dropped, it is
                // still no timeout.
                biased;
                () = exited.cancelled(), if drain.is_none() => {
                    drain = Some(Drain::default());
                    continue;
                }
                () = sleep_until(time_to_timeout), if drain.is_none() => return (stdio, true),
                res = Self::next_message(receiver, drain.as_mut()) => res,
            };
            match res {
                Some(Ok(Message::Data(data, _))) => match stdio.len().checked_add(data.len()) {
                    Some(len) if len < Self::MAX_STDIO_STREAM_SIZE => {
                        stdio.extend_from_slice(&data)
                    }
                    _ => break,
                },
                // A closed channel ends the output as well, it is no timeout.
                Some(Ok(Message::Done) | Err(RecvError)) | None => break,
            }
        }
        (stdio, false)
    }

    /// The next message, bounded by the drain once the process exited.
    async fn next_message(
        receiver: &Receiver<Message>,
        drain: Option<&mut Drain>,
    ) -> Option<Result<Message, RecvError>> {
        match drain {
            Some(drain) => drain.recv(receiver.recv()).await,
            None => Some(receiver.recv().await),
        }
    }

    /// How long a line without its ending waits before it is written as a
    /// partial one, so that prompts and progress output show up in the logs.
    const IDLE_FLUSH: Duration = Duration::from_millis(500);

    /// Read the pipe until it ends and let the log drivers write what they
    /// still buffer, however the loop ended.
    pub async fn read_loop<T>(
        reader: T,
        pipe: Pipe,
        logger: SharedContainerLog,
        message_tx: Option<StdioSender>,
        attach: SharedContainerAttach,
        attach_output: Arc<AttachOutput>,
    ) -> Result<()>
    where
        T: AsyncRead + Unpin,
    {
        let log_has_drivers = logger.read().await.has_drivers();
        let res = Self::read_pipe(reader, pipe, &logger, message_tx, attach).await;
        if log_has_drivers && let Err(e) = logger.write().await.finish(pipe).await {
            error!("Unable to finish the log of {pipe}: {:#}", e);
        }
        // The output ends for the attach endpoints and the exit files only
        // once the last line is in the log.
        drop(attach_output);
        res
    }

    async fn read_pipe<T>(
        mut reader: T,
        pipe: Pipe,
        logger: &SharedContainerLog,
        mut message_tx: Option<StdioSender>,
        attach: SharedContainerAttach,
    ) -> Result<()>
    where
        T: AsyncRead + Unpin,
    {
        let mut buf = BytesMut::with_capacity(READ_BUFFER_SIZE);

        // Cache has_drivers check - drivers are immutable after container creation
        let log_has_drivers = logger.read().await.has_drivers();
        let mut log_errors = LogErrors::default();

        // Whether a line waits for its ending in the log, known from the last
        // write, so that the logger is not locked for it on every read.
        let mut pending = false;

        loop {
            // A large chunk gets split off below and shares the allocation
            // with the consumers, a full buffer is needed for the next read.
            // `read_buf` would only add a few bytes, which splits writes the
            // process made at once.
            buf.reserve(READ_BUFFER_SIZE);

            // A line which waits for its ending gets written once the output
            // pauses, the drivers decide whether they buffer at all.
            let read = if pending {
                select! {
                    read = reader.read_buf(&mut buf) => read,
                    _ = time::sleep(Self::IDLE_FLUSH) => {
                        let res = logger.write().await.flush(pipe).await;
                        log_errors.check(pipe, res.context("flush paused log line"));
                        pending = false;
                        continue;
                    }
                }
            } else {
                reader.read_buf(&mut buf).await
            };

            match read {
                Ok(0) => {
                    debug!("Nothing more to read");

                    Self::forward(&mut message_tx, Message::Done).await;

                    return Ok(());
                }

                Ok(n) => {
                    trace!("Read {} bytes", n);

                    // A small chunk gets copied, so that it does not keep the
                    // whole buffer alive while it is queued and the buffer can
                    // be reused. A large one is split off without a copy.
                    let data = if n < READ_BUFFER_SIZE / 2 {
                        let data = Bytes::copy_from_slice(&buf);
                        buf.clear();
                        data
                    } else {
                        buf.split().freeze()
                    };

                    // Write to logger if drivers are configured
                    // (exec_sync uses an empty logger, so skip the overhead).
                    // A failing driver drops the chunk, the output must keep
                    // flowing to the other drivers and the attach clients.
                    if log_has_drivers {
                        let mut locked_logger = logger.write().await;
                        let res = locked_logger.write(pipe, &data).await;
                        pending = locked_logger.has_pending(pipe);
                        drop(locked_logger);
                        log_errors.check(pipe, res.context("write to log file"));
                    }

                    // Only send to attach if there are active attach clients
                    // Bytes::clone() is just a refcount increment, no data copy
                    if attach.has_readers() {
                        attach.write(Message::Data(data.clone(), pipe));
                    }

                    Self::forward(&mut message_tx, Message::Data(data, pipe)).await;
                }

                Err(e) => match Errno::from_raw(e.raw_os_error().context("get OS error")?) {
                    Errno::EIO => {
                        debug!("Stopping read loop");
                        Self::forward(&mut message_tx, Message::Done).await;

                        return Ok(());
                    }
                    Errno::EBADF => bail!(e),
                    Errno::EAGAIN => continue,
                    _ => return Err(e).context("read from container file descriptor"),
                },
            }
        }
    }

    /// Forward a message to the stdio channel, see [`StdioSender`]. If the
    /// channel got closed or all receivers are gone (see
    /// [`ContainerIO::drop_stdio`]), the sender is dropped as well so that the
    /// channel buffer gets freed and no further sends are attempted.
    async fn forward(message_tx: &mut Option<StdioSender>, message: Message) {
        if let Some(tx) = message_tx
            && !tx.send(message).await
        {
            trace!("Stdio channel closed, dropping sender");
            *message_tx = None;
        }
    }

    pub async fn read_loop_stdin(
        mut writer: impl AsyncWrite + Unpin,
        mut attach: SharedContainerAttach,
        token: CancellationToken,
    ) -> Result<()> {
        // While we're not processing input from a caller, and logically should be able to
        // catch a Message::Done here, it doesn't quite work that way.
        // Every child has an io instance that starts this function, though not
        // all children have input to process. If there is no input from the child
        // then we leak this function, causing memory to balloon over time.
        // Thus, we need the select statement and token. Nothing gets written
        // after the container exited, which could block on descendants holding
        // the pipe open.
        let res = select! {
            res = Self::forward_stdin(&mut writer, &mut attach) => res,
            _ = token.cancelled() => Ok(()),
        };

        // Nothing reads the stdin anymore, so let the senders drop their data
        // instead of waiting for the channel forever.
        attach.close_stdin();
        res
    }

    async fn forward_stdin(
        mut writer: impl AsyncWrite + Unpin,
        attach: &mut SharedContainerAttach,
    ) -> Result<()> {
        let stdin_closed = attach.stdin_closed().clone();
        loop {
            select! {
                res = attach.read() => {
                    let data = res.context("read from stdin attach endpoints")?;
                    Self::handle_stdin_data(&data, &mut writer).await?;
                }
                _ = stdin_closed.cancelled() => break,
            }
        }

        // An attach which closes stdin right after sending data may race with
        // the cancellation, so write everything that was sent before.
        attach.close_stdin();
        while let Ok(data) = attach.try_read() {
            Self::handle_stdin_data(&data, &mut writer).await?;
        }
        Ok(())
    }

    async fn handle_stdin_data(data: &[u8], mut writer: impl AsyncWrite + Unpin) -> Result<()> {
        debug!("Got {} attach bytes", data.len());

        writer
            .write_all(data)
            .await
            .context("write attach stdin to stream")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_log::{ContainerLog, test_helpers};
    use tokio::{
        io::{DuplexStream, duplex},
        task::{self, JoinHandle},
    };

    /// Upper bound for tests which wait for something, it only hits if the
    /// waited for thing never happens.
    const WAIT: Duration = Duration::from_secs(10);

    /// Run a read loop on one end of a pipe and return the other end, the
    /// receiver of its stdio channel and the loop.
    fn spawn_loop(
        logger: SharedContainerLog,
        consumed: bool,
    ) -> (DuplexStream, Receiver<Message>, JoinHandle<Result<()>>) {
        let (writer, reader) = duplex(64 * 1024);
        let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        let sender = StdioSender::new(tx, &Arc::new(AtomicBool::new(consumed)));
        let attach = SharedContainerAttach::default();
        let output = attach.output();
        let read_loop = task::spawn(ContainerIO::read_loop(
            reader,
            Pipe::StdOut,
            logger,
            Some(sender),
            attach,
            output,
        ));
        (writer, rx, read_loop)
    }

    fn data(message: Message) -> Bytes {
        match message {
            Message::Data(data, _) => data,
            Message::Done => panic!("unexpected done"),
        }
    }

    /// Distinct output, so that a lost or reordered chunk is noticed.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn drain_stops_on_busy_channel() {
        let (tx, rx) = async_channel::bounded(16);
        tokio::spawn(async move { while tx.send(()).await.is_ok() {} });

        let mut drain = Drain::default();
        let mut received = 0;
        while drain.recv(rx.recv()).await.is_some() {
            received += 1;
        }
        assert_eq!(received, DRAIN_MAX_MESSAGES);
    }

    #[tokio::test(start_paused = true)]
    async fn drain_stops_after_waiting_in_total() {
        // A process left behind which never pauses long enough for one wait.
        let (tx, rx) = async_channel::bounded(16);
        tokio::spawn(async move {
            while tx.send(()).await.is_ok() {
                time::sleep(DRAIN_TIMEOUT * 9 / 10).await;
            }
        });

        let start = Instant::now();
        let mut drain = Drain::default();
        while drain.recv(rx.recv()).await.is_some() {}
        assert_eq!(start.elapsed(), DRAIN_MAX_WAIT);

        // Time spent forwarding does not count.
        let (tx, rx) = async_channel::bounded(16);
        tx.send(()).await.expect("send");
        let mut drain = Drain::default();
        assert!(drain.recv(rx.recv()).await.is_some());
        time::sleep(2 * DRAIN_MAX_WAIT).await;
        tx.send(()).await.expect("send");
        assert!(drain.recv(rx.recv()).await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn drain_stops_on_quiet_channel() {
        let (_tx, rx) = async_channel::bounded::<()>(1);
        let start = Instant::now();
        assert!(Drain::default().recv(rx.recv()).await.is_none());
        assert_eq!(start.elapsed(), DRAIN_TIMEOUT);
    }

    #[test]
    fn early_stream_is_bounded_without_gaps() {
        let mut stream = EarlyStream::default();
        let chunk = vec![1; ContainerIO::MAX_STDIO_STREAM_SIZE / 4 + 1];
        for _ in 0..3 {
            stream.push(&chunk);
        }
        assert!(!stream.truncated);
        assert_eq!(stream.data.len(), 3 * chunk.len());

        // A chunk which does not fit gets dropped, and so does everything
        // after it, even if it would fit.
        stream.push(&chunk);
        assert!(stream.truncated);
        stream.push(b"x");
        assert_eq!(stream.data.len(), 3 * chunk.len());
        assert!(stream.data.capacity() <= ContainerIO::MAX_STDIO_STREAM_SIZE);

        // The end stays known for the error of a runtime.
        for i in 0..100u8 {
            stream.push(&vec![i; 1024]);
        }
        let end = stream.end();
        assert_eq!(end.len(), EarlyStream::TAIL_SIZE);
        assert_eq!(end.last(), Some(&99));
    }

    #[test]
    fn early_stream_end_spans_kept_and_dropped_output() {
        let mut stream = EarlyStream::default();
        stream.push(&vec![b'a'; ContainerIO::MAX_STDIO_STREAM_SIZE - 10]);
        // Does not fit anymore, only a short part got dropped.
        stream.push(b"0123456789abcdefghij");
        assert!(stream.truncated);

        let end = stream.end();
        assert_eq!(end.len(), EarlyStream::TAIL_SIZE);
        assert!(end.ends_with(b"a0123456789abcdefghij"));
    }

    #[tokio::test]
    async fn collect_drops_disabled_output() {
        let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        for message in [
            Message::Data(Bytes::from_static(b"x"), Pipe::StdOut),
            Message::Done,
        ] {
            tx.send(message).await.expect("send");
        }
        let stream = ContainerIO::collect(&rx, &CancellationToken::new(), false).await;
        assert!(stream.data.is_empty());
        assert!(stream.done);
    }

    #[tokio::test]
    async fn read_stream_does_not_append_to_truncated_output() {
        let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        tx.send(Message::Data(Bytes::from_static(b"later"), Pipe::StdOut))
            .await
            .expect("send");
        let early = EarlyStream {
            data: b"early".to_vec(),
            done: false,
            truncated: true,
            tail: Tail::default(),
        };
        let (stdio, timed_out) =
            ContainerIO::read_stream(None, &CancellationToken::new(), &rx, early).await;
        assert_eq!(stdio, b"early");
        assert!(!timed_out);
        assert!(rx.is_closed());
    }

    #[tokio::test]
    async fn exec_output_during_runtime_is_kept() -> Result<()> {
        let mut io = ContainerIO::new(false, ContainerLog::new())?;
        io.collect_stdio(true, true)?;

        // More chunks than the channel holds, all written before the runtime
        // returned and anything read the output.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "for i in $(seq 1 300); do echo $i; sleep 0.001; done"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let token = CancellationToken::new();
        match io.typ_mut() {
            ContainerIOType::Streams(s) => s.handle_stdio_receive(
                None,
                child.stdout.take(),
                child.stderr.take(),
                token.clone(),
            ),
            ContainerIOType::Terminal(_) => bail!("no streams"),
        }
        child.wait().await?;
        time::sleep(Duration::from_millis(100)).await;

        token.cancel();
        let (stdout, _, timed_out) = io.read_all_with_timeout(None, &token).await?;
        assert!(!timed_out);
        let expected: String = (1..=300).map(|i| format!("{i}\n")).collect();
        assert_eq!(String::from_utf8(stdout)?, expected);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn read_stream_after_exit_is_bounded_in_time() {
        // A process left behind which never pauses long enough for the drain.
        let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        tokio::spawn(async move {
            while tx
                .send(Message::Data(Bytes::from_static(b"x"), Pipe::StdOut))
                .await
                .is_ok()
            {
                time::sleep(DRAIN_TIMEOUT * 9 / 10).await;
            }
        });

        let exited = CancellationToken::new();
        exited.cancel();
        let start = Instant::now();
        let deadline = start + Duration::from_secs(10);
        let (stdio, timed_out) =
            ContainerIO::read_stream(Some(deadline), &exited, &rx, EarlyStream::default()).await;
        assert!(!timed_out);
        assert_eq!(start.elapsed(), DRAIN_MAX_WAIT);
        assert_eq!(stdio.len(), 12);
        assert!(rx.is_closed());
    }

    #[tokio::test(start_paused = true)]
    async fn read_loop_flushes_paused_line() -> Result<()> {
        let (cri, json, logger) = test_helpers::container_log(true).await?;
        let (mut writer, _rx, read_loop) = spawn_loop(logger, false);

        writer.write_all(b"prompt> ").await?;
        // The CRI log writes partial lines right away, the JSON one only once
        // the output paused.
        let mut flushed = false;
        for _ in 0..100 {
            time::sleep(ContainerIO::IDLE_FLUSH / 10).await;
            if test_helpers::read(&json)?.contains(r#""message":"prompt> ","partial":true"#) {
                flushed = true;
                break;
            }
        }
        assert!(flushed, "paused line not flushed");
        assert_eq!(
            test_helpers::read(&cri)?,
            format!("{} stdout P prompt> \n", test_helpers::TIMESTAMP)
        );

        // The continuation is a line of its own and nothing is written twice.
        writer.write_all(b"ls\n").await?;
        drop(writer);
        read_loop.await??;
        let json = test_helpers::read(&json)?;
        assert_eq!(json.lines().count(), 2, "{json}");
        assert!(json.contains(r#""message":"ls""#));
        Ok(())
    }

    #[tokio::test]
    async fn last_line_is_logged_before_the_output_ends() -> Result<()> {
        let (_cri, json, logger) = test_helpers::container_log(true).await?;
        let attach = SharedContainerAttach::default();
        let (mut rx, _) = attach.subscribe();
        let (mut writer, reader) = duplex(1024);
        let read_loop = task::spawn(ContainerIO::read_loop(
            reader,
            Pipe::StdOut,
            logger.clone(),
            None,
            attach.clone(),
            attach.output(),
        ));

        writer.write_all(b"no newline").await?;
        drop(writer);
        // The exit files get written once the output ended, the log must not
        // be busy with the last line anymore then.
        while time::timeout(WAIT, rx.recv()).await?? != Message::Done {}
        assert!(logger.try_write().is_ok());
        assert!(test_helpers::read(&json)?.contains(r#""message":"no newline","partial":true"#));
        read_loop.await?
    }

    #[tokio::test]
    async fn log_errors_do_not_stop_the_loop() -> Result<()> {
        // The drivers are not opened, so every write fails.
        let (_cri, _json, logger) = test_helpers::container_log(false).await?;
        let (mut writer, rx, read_loop) = spawn_loop(logger, true);

        writer.write_all(b"first\n").await?;
        assert_eq!(data(rx.recv().await?), "first\n");
        writer.write_all(b"second\n").await?;
        assert_eq!(data(rx.recv().await?), "second\n");
        drop(writer);
        assert_eq!(rx.recv().await?, Message::Done);
        read_loop.await?
    }

    #[tokio::test]
    async fn consumer_gets_all_output() -> Result<()> {
        let (mut writer, rx, read_loop) = spawn_loop(ContainerLog::new(), true);

        // Far more chunks than the channel holds.
        let expected = pattern(STDIO_CHANNEL_SIZE * READ_BUFFER_SIZE * 2);
        let input = expected.clone();
        let write = task::spawn(async move { writer.write_all(&input).await });

        let mut got = vec![];
        loop {
            // A slow consumer.
            task::yield_now().await;
            match time::timeout(WAIT, rx.recv()).await?? {
                Message::Data(data, _) => got.extend_from_slice(&data),
                Message::Done => break,
            }
        }
        write.await??;
        assert!(got == expected, "output lost or reordered");
        read_loop.await?
    }

    #[tokio::test]
    async fn absent_consumer_does_not_block_output() -> Result<()> {
        // Nobody reads the channel, its receiver stays around.
        let (mut writer, _rx, read_loop) = spawn_loop(ContainerLog::new(), false);

        let output = pattern(STDIO_CHANNEL_SIZE * READ_BUFFER_SIZE * 2);
        time::timeout(WAIT, writer.write_all(&output)).await??;
        drop(writer);
        time::timeout(WAIT, read_loop).await??
    }

    #[tokio::test]
    async fn reads_full_chunks_after_large_one() -> Result<()> {
        let (mut writer, rx, read_loop) = spawn_loop(ContainerLog::new(), true);

        // A large chunk is split off without a copy, the next read must still
        // get a whole buffer.
        writer.write_all(&pattern(READ_BUFFER_SIZE * 3 / 4)).await?;
        assert_eq!(data(rx.recv().await?).len(), READ_BUFFER_SIZE * 3 / 4);
        writer.write_all(&pattern(READ_BUFFER_SIZE)).await?;
        assert_eq!(data(rx.recv().await?).len(), READ_BUFFER_SIZE);

        drop(writer);
        assert_eq!(rx.recv().await?, Message::Done);
        read_loop.await?
    }

    #[tokio::test]
    async fn exec_output_is_capped_without_blocking() -> Result<()> {
        let (mut writer, rx, read_loop) = spawn_loop(ContainerLog::new(), true);

        let output = pattern(ContainerIO::MAX_STDIO_STREAM_SIZE + 2 * 1024 * 1024);
        let write = task::spawn(async move {
            writer.write_all(&output).await?;
            // The process keeps writing after the limit.
            writer.write_all(&output).await
        });

        let (stdout, timed_out) =
            ContainerIO::read_stream(None, &CancellationToken::new(), &rx, EarlyStream::default())
                .await;
        assert!(!timed_out);
        assert!(stdout.len() < ContainerIO::MAX_STDIO_STREAM_SIZE);
        // At most the chunk which crossed the limit is missing.
        assert!(stdout.len() > ContainerIO::MAX_STDIO_STREAM_SIZE - 64 * 1024);
        assert!(stdout == pattern(stdout.len()));

        // The rest gets discarded instead of waiting for the consumer.
        time::timeout(WAIT, write).await???;
        time::timeout(WAIT, read_loop).await??
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_returns_partial_output() -> Result<()> {
        let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        tx.send(Message::Data("partial".into(), Pipe::StdOut))
            .await?;

        let deadline = Instant::now() + Duration::from_secs(5);
        let (stdout, timed_out) = ContainerIO::read_stream(
            Some(deadline),
            &CancellationToken::new(),
            &rx,
            EarlyStream::default(),
        )
        .await;
        assert_eq!(stdout, b"partial");
        assert!(timed_out);
        assert_eq!(Instant::now(), deadline);
        // The channel got closed for the read loop.
        assert!(tx.is_closed());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn exit_ends_read_while_pipe_is_held_open() -> Result<()> {
        // The sender stays, like a background process holding the pipe.
        let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        let exited = CancellationToken::new();
        tx.send(Message::Data("before ".into(), Pipe::StdOut))
            .await?;

        let read = task::spawn({
            let exited = exited.clone();
            async move {
                let deadline = Instant::now() + Duration::from_secs(60);
                ContainerIO::read_stream(Some(deadline), &exited, &rx, EarlyStream::default()).await
            }
        });
        task::yield_now().await;
        exited.cancel();
        // Output which was still in flight at the exit is taken.
        tx.send(Message::Data("exit".into(), Pipe::StdOut)).await?;

        let start = Instant::now();
        let (stdout, timed_out) = read.await?;
        assert_eq!(stdout, b"before exit");
        assert!(!timed_out);
        assert!(start.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn exit_before_the_deadline_is_no_timeout() -> Result<()> {
        let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        let exited = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(1);
        let read = task::spawn({
            let exited = exited.clone();
            async move {
                ContainerIO::read_stream(Some(deadline), &exited, &rx, EarlyStream::default()).await
            }
        });

        time::sleep(Duration::from_millis(950)).await;
        tx.send(Message::Data("before ".into(), Pipe::StdOut))
            .await?;
        exited.cancel();
        // The output still drains past the deadline.
        time::sleep(Duration::from_millis(80)).await;
        tx.send(Message::Data("exit".into(), Pipe::StdOut)).await?;

        let (stdout, timed_out) = read.await?;
        assert_eq!(stdout, b"before exit");
        assert!(!timed_out);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn closed_channel_is_no_timeout() -> Result<()> {
        let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        tx.send(Message::Data("data".into(), Pipe::StdOut)).await?;
        drop(tx);

        let deadline = Instant::now() + Duration::from_secs(5);
        let (stdout, timed_out) = ContainerIO::read_stream(
            Some(deadline),
            &CancellationToken::new(),
            &rx,
            EarlyStream::default(),
        )
        .await;
        assert_eq!(stdout, b"data");
        assert!(!timed_out);
        Ok(())
    }
}
