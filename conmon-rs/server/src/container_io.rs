//! The stdio of the containers and execs, pipes or a terminal, which the
//! server reads into the log drivers, the attach clients and the output of the
//! execs.

use crate::{
    attach::{AttachOutput, SharedContainerAttach},
    child::sleep_until,
    child_reaper::with_stderr,
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

/// Upper bound for the time the exit files wait for the log drivers to take
/// the output after the process exited, which does not count as a pause of
/// the output, see `SharedContainerAttach::logging`, and for the read loops
/// to end once stopped, see `ReapableChild::finish_output`. A journal which
/// stalls takes up to two seconds per chunk, a pipe full of them must not
/// keep the exit from being reported for minutes.
pub(crate) const DRAIN_MAX_LOG_WAIT: Duration = Duration::from_secs(5);

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

    /// Whether the drain forwarded or waited as much as it may.
    pub(crate) fn exhausted(&self) -> bool {
        self.remaining == 0 || self.waited >= DRAIN_MAX_WAIT
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
                    warn!("Attach client too slow, skipped {n} messages");
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("Skipped {n} attach output messages");
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

    /// Read the output of an exec, see [`ContainerIO::read_all`].
    pub async fn read_all(&self, exited: &CancellationToken) -> Result<(Vec<u8>, Vec<u8>)> {
        self.0.write().await.read_all(exited).await
    }

    /// Resize the shared container IO to the provided width and height.
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
    pub fn as_str(self) -> &'static str {
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
    /// The most output of a pipe an exec collects while its runtime runs,
    /// see [`ContainerIO::collect_stdio`], and the most an `exec_sync`
    /// returns.
    pub(crate) const MAX_STDIO_STREAM_SIZE: usize = 16 * 1024 * 1024;

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
    /// use the attach broadcast, so they get dropped, see
    /// [`ContainerIO::close_stdio`].
    pub fn drop_stdio(&mut self) {
        match self.typ_mut() {
            ContainerIOType::Terminal(t) => Self::close_stdio(t.message_rx_mut().take()),
            ContainerIOType::Streams(s) => s.drop_stdio(),
        }
    }

    /// Close a stdio channel and drop what it buffers right away. The read
    /// loop drops its sender on the next send, a quiet container would keep
    /// the buffered chunks until then otherwise.
    pub fn close_stdio(rx: Option<Receiver<Message>>) {
        if let Some(rx) = rx {
            rx.close();
            while rx.try_recv().is_ok() {}
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
    /// a terminal, the error carries it already. The read loops get stopped
    /// afterwards, a process which the runtime left behind, like a daemon of
    /// the process it started, may keep the pipes open, see
    /// [`SharedContainerAttach::stop_output`].
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
                    match self.read_all_with_early(&exited, early).await {
                        Ok((_, stderr)) => stderr,
                        Err(read_err) => {
                            debug!("Unable to read runtime output: {read_err:#}");
                            vec![]
                        }
                    }
                }
            }
            ContainerIOType::Terminal(_) => vec![],
        };
        self.attach.stop_output();
        with_stderr(e, &stderr)
    }

    /// Read the output until it ended. Once `exited` got cancelled, the
    /// output still in flight is taken as long as the [`Drain`] does. A
    /// background process which holds the pipes open must not keep this
    /// waiting. The channels get closed afterwards, so that the read loops
    /// discard what follows. There is no deadline of its own: the watcher of
    /// an exec cancels `exited` also when it kills the process at its
    /// timeout, and a deadline which raced that one would end the output of
    /// a process which exited right at it.
    pub async fn read_all(&mut self, exited: &CancellationToken) -> Result<(Vec<u8>, Vec<u8>)> {
        self.consume_stdio();
        let early = self.take_early_stdio().await;
        self.read_all_with_early(exited, early).await
    }

    /// Read the output after the one collected before, see
    /// [`ContainerIO::read_all`].
    async fn read_all_with_early(
        &mut self,
        exited: &CancellationToken,
        early: EarlyStdio,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        match self.typ_mut() {
            ContainerIOType::Terminal(t) => {
                if let Some(message_rx) = t.message_rx_mut() {
                    let stdout = Self::read_stream(exited, message_rx, early.stdout).await;
                    Ok((stdout, vec![]))
                } else {
                    bail!("read_all called before message_rx was registered");
                }
            }
            ContainerIOType::Streams(s) => {
                let (stdout_rx, stderr_rx) = s.stdio_mut()?;
                Ok(tokio::join!(
                    Self::read_stream(exited, stdout_rx, early.stdout),
                    Self::read_stream(exited, stderr_rx, early.stderr),
                ))
            }
        }
    }

    /// Read a stream, after the `early` output collected before.
    async fn read_stream(
        exited: &CancellationToken,
        receiver: &Receiver<Message>,
        early: EarlyStream,
    ) -> Vec<u8> {
        // Also after the size limit, a process which keeps writing must not
        // wait for this consumer.
        let _close = CloseOnDrop(receiver);

        // The collector kept below the limit. Without it, start with 4 KiB,
        // which handles most exec_sync outputs, doubling handles growth.
        let mut stdio = early.data;
        if early.done || early.truncated {
            return stdio;
        }
        stdio.reserve(4096);
        let mut drain = None;
        loop {
            let res = select! {
                // Once the process exited, the drain bounds the reading,
                // output after it gave up is dropped.
                biased;
                () = exited.cancelled(), if drain.is_none() => {
                    drain = Some(Drain::default());
                    continue;
                }
                res = Self::next_message(receiver, drain.as_mut()) => res,
            };
            match res {
                Some(Ok(Message::Data(data, _))) => match stdio.len().checked_add(data.len()) {
                    Some(len) if len < Self::MAX_STDIO_STREAM_SIZE => {
                        stdio.extend_from_slice(&data);
                    }
                    _ => break,
                },
                // A closed channel ends the output as well.
                Some(Ok(Message::Done) | Err(RecvError)) | None => break,
            }
        }
        stdio
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
        let res =
            Self::read_pipe(reader, pipe, &logger, log_has_drivers, message_tx, &attach).await;
        if log_has_drivers {
            // The line which waits for its ending is on its way into the log
            // like a chunk, see `SharedContainerAttach::logging`.
            let _logging = attach.logging();
            if let Err(e) = logger.write().await.finish(pipe).await {
                error!("Unable to finish the log of {pipe}: {e:#}");
            }
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
        log_has_drivers: bool,
        mut message_tx: Option<StdioSender>,
        attach: &SharedContainerAttach,
    ) -> Result<()>
    where
        T: AsyncRead + Unpin,
    {
        let mut buf = BytesMut::with_capacity(READ_BUFFER_SIZE);
        let mut log_errors = LogErrors::default();
        let stopped = attach.output_stopped().clone();

        // When the line which waits for its ending in the log gets written as
        // a partial one, known from the last write, so that the logger is not
        // locked for it on every read. Set once the line started, a line
        // which trickles in must not wait for a pause of the output.
        let mut flush_at = None;

        loop {
            // A large chunk gets split off below and shares the allocation
            // with the consumers, a full buffer is needed for the next read.
            // `read_buf` would only add a few bytes, which splits writes the
            // process made at once.
            buf.reserve(READ_BUFFER_SIZE);

            // Once stopped, nothing gets read anymore, also if more is there.
            let read = select! {
                biased;
                () = stopped.cancelled() => {
                    debug!("Stopping read loop, nothing takes the output anymore");
                    Self::forward(&mut message_tx, Message::Done).await;
                    return Ok(());
                }
                read = reader.read_buf(&mut buf) => read,
                // The drivers decide whether they buffer lines at all.
                () = sleep_until(flush_at) => {
                    let _logging = attach.logging();
                    let res = logger.write().await.flush(pipe).await;
                    log_errors.check(pipe, res.context("flush paused log line"));
                    flush_at = None;
                    continue;
                }
            };

            match read {
                Ok(0) => {
                    debug!("Nothing more to read");

                    Self::forward(&mut message_tx, Message::Done).await;

                    return Ok(());
                }

                Ok(n) => {
                    trace!("Read {} bytes", n);

                    // Until it got broadcast below, the output is on its way,
                    // see `SharedContainerAttach::logging`.
                    let logging = log_has_drivers.then(|| attach.logging());

                    // Write to logger if drivers are configured
                    // (exec_sync uses an empty logger, so skip the overhead).
                    // A failing driver drops the chunk, the output must keep
                    // flowing to the other drivers and the attach clients.
                    if log_has_drivers {
                        let mut locked_logger = logger.write().await;
                        let res = locked_logger.write(pipe, &buf).await;
                        let pending = locked_logger.has_pending(pipe);
                        drop(locked_logger);
                        log_errors.check(pipe, res.context("write to log file"));

                        // A newline in the chunk ended the line which waited
                        // before, the one waiting now started in this chunk.
                        flush_at = match flush_at {
                            _ if !pending => None,
                            Some(at) if memchr::memchr(b'\n', &buf).is_none() => Some(at),
                            _ => Some(Instant::now() + Self::IDLE_FLUSH),
                        };
                    }

                    // Only the attach clients and the stdio channel take the
                    // chunk along, the buffer gets reused otherwise.
                    let attached = attach.has_readers();
                    if !attached && message_tx.is_none() {
                        buf.clear();
                        continue;
                    }

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

                    // Bytes::clone() is just a refcount increment, no data copy
                    if attached {
                        attach.write(Message::Data(data.clone(), pipe));
                    }
                    drop(logging);

                    Self::forward(&mut message_tx, Message::Data(data, pipe)).await;
                }

                Err(e) => match Errno::from_raw(e.raw_os_error().context("get OS error")?) {
                    Errno::EIO => {
                        debug!("Stopping read loop");
                        Self::forward(&mut message_tx, Message::Done).await;

                        return Ok(());
                    }
                    Errno::EBADF => bail!(e),
                    Errno::EAGAIN => {}
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
        attach: SharedContainerAttach,
        token: CancellationToken,
    ) -> Result<()> {
        // The exit of the container cancels the token. Without it the loop would
        // wait for attach input forever, which a container may never get.
        // Nothing gets written after the exit either, which could block on
        // descendants holding the pipe open.
        let res = select! {
            res = Self::forward_stdin(&mut writer, &attach) => res,
            () = token.cancelled() => Ok(()),
        };

        // Nothing reads the stdin anymore, so let the senders drop their data
        // instead of waiting for the channel forever.
        attach.close_stdin();
        res
    }

    async fn forward_stdin(
        mut writer: impl AsyncWrite + Unpin,
        attach: &SharedContainerAttach,
    ) -> Result<()> {
        let stdin_closed = attach.stdin_closed().clone();
        loop {
            select! {
                res = attach.read() => {
                    let data = res.context("read from stdin attach endpoints")?;
                    Self::handle_stdin_data(&data, &mut writer).await?;
                }
                () = stdin_closed.cancelled() => break,
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
mod tests;
