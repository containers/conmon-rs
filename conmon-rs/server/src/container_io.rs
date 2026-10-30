//! The stdio of the containers and execs, pipes or a terminal, which the
//! server reads into the log drivers, the attach clients and the output of the
//! execs.

use crate::{
    attach::{AttachOutput, SharedContainerAttach},
    container_log::SharedContainerLog,
    mutex::lock,
    random_id::RandomId,
    tail::Tail,
    terminal::Terminal,
    util::{sleep_until, with_stderr},
};
use anyhow::{Context, Result, bail};
use async_channel::{Receiver, RecvError, Sender};
use bytes::Bytes;
use nix::errno::Errno;
use std::{
    cell::RefCell,
    fmt, future,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Poll, ready},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf},
    process::{ChildStderr, ChildStdin, ChildStdout},
    select,
    sync::broadcast,
    task::{self, JoinHandle},
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, info, trace, warn};

/// How many read chunks the stdio and terminal channels buffer per pipe.
pub const STDIO_CHANNEL_SIZE: usize = 100;

/// The most a read of a container I/O stream takes at once. The read loops
/// read into the scratch buffer of their thread and copy each chunk out of
/// it, see [`with_scratch`], so an idle container holds no read buffer. 8 KiB
/// halves the reads and log writes of 4 KiB for bulk output and keeps what a
/// container holds after it in line with the log buffers, see
/// `LogFile::BUF_CAPACITY`.
pub(crate) const READ_BUFFER_SIZE: usize = 8 * 1024;

thread_local! {
    /// See [`with_scratch`].
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` on a buffer of `len` bytes which the thread keeps for all reads
/// which copy what they got out of it right away. The read loops of all
/// containers and attach clients share it, instead of keeping a buffer each
/// while they wait. It grows to the largest `len` asked for and holds what
/// the last use left in it.
pub(crate) fn with_scratch<R>(len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    SCRATCH.with(|scratch| match scratch.try_borrow_mut() {
        Ok(mut scratch) => {
            if scratch.len() < len {
                scratch.resize(len, 0);
            }
            f(&mut scratch[..len])
        }
        // Only if `f` used it again, which none of the reads does.
        Err(_) => f(&mut vec![0; len]),
    })
}

/// Upper bound for waiting on the next output after the process exited. The
/// read loops may still be draining the pipes when the exit is observed, they
/// signal the end with `Message::Done`. The timeout only kicks in if that
/// never arrives, for example because a grandchild keeps the pipe open.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

/// Upper bound for the messages forwarded per drain after the process exited.
/// The output written before the exit is at most what the channels (up to
/// 128 messages) and the pipe buffers hold. With the default pipe-max-size of
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
    /// receiver which fell behind skips messages, which gets logged, for
    /// the receiver of a client with its [`Lag`], the one it kept before
    /// the exit. None once the output ended or the drain gave up.
    pub(crate) async fn recv_output(
        &mut self,
        rx: &mut broadcast::Receiver<Message>,
        mut lag: Option<&mut Lag>,
    ) -> Option<Message> {
        loop {
            match self.recv(rx.recv()).await? {
                Ok(Message::Done) | Err(broadcast::error::RecvError::Closed) => return None,
                Ok(message) => {
                    if let Some(lag) = lag.as_deref_mut() {
                        lag.received(rx);
                    }
                    return Some(message);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => match lag.as_deref_mut() {
                    Some(lag) => lag.skipped(n),
                    None => debug!("Skipped {n} attach output messages"),
                },
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

    /// Whether a consumer reads the channel, see
    /// [`ContainerIO::consume_stdio`].
    fn consumed(&self) -> bool {
        self.consumed.load(Ordering::Acquire)
    }

    /// Send a message, false if the channel got closed or all receivers are
    /// gone.
    async fn send(&self, message: Message) -> bool {
        if self.consumed() {
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

/// Logs an attach client which falls behind the output once until it
/// caught up again, like [`LogErrors`]. A client which stays too slow skips
/// messages again every time the channel filled up, which would log a
/// warning for every one of its capacity of messages otherwise.
#[derive(Debug, Default)]
pub(crate) struct Lag {
    /// The messages skipped since the client fell behind, none while it
    /// keeps up.
    skipped: Option<u64>,
}

impl Lag {
    /// The client skipped `n` messages.
    pub(crate) fn skipped(&mut self, n: u64) {
        match &mut self.skipped {
            Some(skipped) => {
                *skipped = skipped.saturating_add(n);
                debug!("Attach client still too slow, skipped {skipped} messages so far");
            }
            None => {
                self.skipped = Some(n);
                warn!("Attach client too slow, skipped {n} messages, more until it catches up");
            }
        }
    }

    /// The client received a message from `rx`. It caught up once nothing
    /// is queued for it anymore.
    pub(crate) fn received<T>(&mut self, rx: &broadcast::Receiver<T>) {
        if self.skipped.is_some()
            && rx.is_empty()
            && let Some(skipped) = self.skipped.take()
        {
            debug!("Attach client caught up after skipping {skipped} messages");
        }
    }
}

/// A container IO shared by its users, like the watcher, the attach
/// endpoints and the consumer of the output of an exec.
pub type SharedContainerIO = Arc<ContainerIO>;

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
    /// None for a process with pipes.
    terminal: Option<Terminal>,
    logger: SharedContainerLog,
    attach: SharedContainerAttach,

    /// The stdio channels, see [`StdioSender`]. A terminal merges stderr
    /// into stdout, its stderr channel ends right away.
    stdout: StdioChannel,
    stderr: StdioChannel,

    /// Whether a consumer reads the stdio channels, see [`StdioSender`].
    consumed: Arc<AtomicBool>,

    /// Collects the output of an exec while its runtime runs, see
    /// [`ContainerIO::collect_stdio`]. Taken by the consumer of the output,
    /// which shares the IO with others already.
    collector: Mutex<Option<StdioCollector>>,
}

/// A stdio channel of a pipe, see [`StdioSender`].
#[derive(Debug)]
struct StdioChannel {
    /// Until the channel got closed, consumers read clones of it.
    rx: Option<Receiver<Message>>,

    /// Until the read loop of the pipe took it, once the process got
    /// connected.
    tx: Option<StdioSender>,
}

impl StdioChannel {
    fn new(capacity: usize, consumed: &Arc<AtomicBool>) -> Self {
        let (tx, rx) = async_channel::bounded(capacity);
        Self {
            rx: Some(rx),
            tx: Some(StdioSender::new(tx, consumed)),
        }
    }

    /// Close the channel and drop what it buffers right away, a quiet
    /// container would keep the buffered chunks otherwise. The read loop
    /// drops its sender once it notices, which frees the channel.
    fn close(&mut self) {
        self.tx = None;
        if let Some(rx) = self.rx.take() {
            rx.close();
            while rx.try_recv().is_ok() {}
        }
    }
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
    /// The terminal of the process, none with pipes.
    pub fn terminal(&self) -> Option<&Terminal> {
        self.terminal.as_ref()
    }
    pub fn logger(&self) -> &SharedContainerLog {
        &self.logger
    }
    pub fn attach(&self) -> &SharedContainerAttach {
        &self.attach
    }

    /// Resize the terminal. Errors in case of no terminal containers.
    pub fn resize(&self, width: u16, height: u16) -> Result<()> {
        if !self.try_resize(width, height)? {
            bail!("container has no terminal")
        }
        Ok(())
    }

    /// Resize the terminal if the container has one and return whether it
    /// does.
    pub fn try_resize(&self, width: u16, height: u16) -> Result<bool> {
        let Some(terminal) = &self.terminal else {
            return Ok(false);
        };
        terminal.resize(width, height).context("resize terminal")?;
        Ok(true)
    }

    /// The end of file character of the terminal, none without one.
    pub fn terminal_eof(&self) -> Option<u8> {
        self.terminal.as_ref().map(Terminal::eof_char)
    }
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

impl ContainerIO {
    /// The most output of a pipe an exec collects while its runtime runs,
    /// see [`ContainerIO::collect_stdio`], and the most an `exec_sync`
    /// returns.
    pub(crate) const MAX_STDIO_STREAM_SIZE: usize = 16 * 1024 * 1024;

    /// Create a new container IO instance.
    pub fn new(terminal: bool, logger: SharedContainerLog) -> Result<Self> {
        let terminal = terminal
            .then(Terminal::new)
            .transpose()
            .context("create new terminal")?;
        let consumed = Arc::default();
        let stdout = StdioChannel::new(STDIO_CHANNEL_SIZE, &consumed);
        // The channel preallocates its capacity. The one of stderr carries
        // nothing with a terminal. An exec keeps it as long as the IO, only
        // its consumer closes it.
        let stderr = if terminal.is_some() {
            let mut stderr = StdioChannel::new(1, &consumed);
            stderr.tx = None;
            stderr
        } else {
            StdioChannel::new(STDIO_CHANNEL_SIZE, &consumed)
        };
        Ok(Self {
            attach: SharedContainerAttach::new(terminal.is_some()),
            terminal,
            logger,
            stdout,
            stderr,
            consumed,
            collector: Mutex::default(),
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
        let (stdout, stderr) = self.receivers()?;
        Ok((stdout.clone(), stderr.clone()))
    }

    /// The stdout and stderr receivers, see [`ContainerIO::stdio`].
    fn receivers(&self) -> Result<(&Receiver<Message>, &Receiver<Message>)> {
        match (&self.stdout.rx, &self.stderr.rx) {
            (Some(stdout), Some(stderr)) => Ok((stdout, stderr)),
            _ => bail!("stdio channels have been dropped"),
        }
    }

    /// Drop the stdout and stderr channels.
    ///
    /// The channels buffer the last chunks per pipe for consumers like
    /// `exec_sync`, which may start reading after the process already produced
    /// output, and for reporting the runtime's stderr if a create fails. Once
    /// a container is created nothing consumes them anymore, attach sessions
    /// use the attach broadcast, so they get dropped, see
    /// [`StdioChannel::close`].
    pub fn drop_stdio(&mut self) {
        self.stdout.close();
        self.stderr.close();
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
    pub fn collect_stdio(&self, stdout_enabled: bool, stderr_enabled: bool) -> Result<()> {
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
        *lock(&self.collector) = Some(StdioCollector {
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
    pub async fn take_early_stdio(&self) -> EarlyStdio {
        let Some(mut collector) = lock(&self.collector).take() else {
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
        self.consumed.store(true, Ordering::Release);
    }

    /// Attach the stderr output of a runtime which failed to its error. With
    /// a terminal, the error carries it already. The read loops get stopped
    /// afterwards, a process which the runtime left behind, like a daemon of
    /// the process it started, may keep the pipes open, see
    /// [`SharedContainerAttach::stop_output`].
    pub async fn runtime_error(&self, e: anyhow::Error) -> anyhow::Error {
        let stderr = match self.terminal {
            None => {
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
            Some(_) => vec![],
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
    pub async fn read_all(&self, exited: &CancellationToken) -> Result<(Vec<u8>, Vec<u8>)> {
        self.consume_stdio();
        let early = self.take_early_stdio().await;
        self.read_all_with_early(exited, early).await
    }

    /// Read the output after the one collected before, see
    /// [`ContainerIO::read_all`].
    async fn read_all_with_early(
        &self,
        exited: &CancellationToken,
        early: EarlyStdio,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let (stdout, stderr) = self.receivers()?;
        Ok(tokio::join!(
            Self::read_stream(exited, stdout, early.stdout),
            Self::read_stream(exited, stderr, early.stderr),
        ))
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

    /// Read the pipes of the runtime, which the process it starts shares.
    pub fn connect_streams(
        &mut self,
        stdin: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        token: CancellationToken,
    ) {
        debug!("Start reading from IO streams");
        // Shared by both read loops, the output ends once the last one ended.
        let output = self.attach.output();
        self.spawn_stdin_loop(stdin, token);
        if let Some(stdout) = stdout {
            self.spawn_read_loop(stdout, Pipe::StdOut, output.clone());
        }
        if let Some(stderr) = stderr {
            self.spawn_read_loop(stderr, Pipe::StdErr, output);
        }
    }

    /// Wait until the runtime connected the terminal and read it. A wait
    /// which gets dropped leaves the connection for the next one.
    pub async fn connect_terminal(&mut self, stdin: bool, token: CancellationToken) -> Result<()> {
        let terminal = self
            .terminal
            .as_mut()
            .context("container has no terminal")?;
        let tty = match terminal.connect().await {
            Ok(tty) => tty,
            Err(e) => {
                // Nothing will ever read the stdin, so attach clients must
                // not wait for it.
                self.attach.close_stdin();
                return Err(e);
            }
        };
        let output = self.attach.output();
        self.spawn_read_loop(tty.clone(), Pipe::StdOut, output);
        self.spawn_stdin_loop(stdin.then_some(tty), token);
        Ok(())
    }

    /// Spawn the [`ContainerIO::read_loop`] of a pipe, which takes the
    /// sender of its stdio channel.
    fn spawn_read_loop<T>(&mut self, reader: T, pipe: Pipe, output: Arc<AttachOutput>)
    where
        T: AsyncRead + Unpin + Send + 'static,
    {
        let channel = match pipe {
            Pipe::StdOut => &mut self.stdout,
            Pipe::StdErr => &mut self.stderr,
        };
        let read_loop = Self::read_loop(
            reader,
            pipe,
            self.logger.clone(),
            channel.tx.take(),
            self.attach.clone(),
            output,
        );
        task::spawn(
            async move {
                if let Err(e) = read_loop.await {
                    error!("Read loop of {pipe} failed: {e:#}");
                }
            }
            .instrument(debug_span!("read_loop", %pipe)),
        );
    }

    /// Spawn the [`ContainerIO::read_loop_stdin`] of the process. Without a
    /// stdin, attach clients must not wait for one which nothing ever reads.
    fn spawn_stdin_loop<T>(&self, writer: Option<T>, token: CancellationToken)
    where
        T: AsyncWrite + Unpin + Send + 'static,
    {
        let Some(writer) = writer else {
            self.attach.close_stdin();
            return;
        };
        let attach = self.attach.clone();
        task::spawn(
            async move {
                if let Err(e) = Self::read_loop_stdin(writer, attach, token).await {
                    error!("Stdin read loop failure: {e:#}");
                }
            }
            .instrument(debug_span!("read_loop_stdin")),
        );
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
        let log_has_drivers = logger.lock().await.has_drivers();
        let res =
            Self::read_pipe(reader, pipe, &logger, log_has_drivers, message_tx, &attach).await;
        if log_has_drivers {
            // The line which waits for its ending is on its way into the log
            // like a chunk, see `SharedContainerAttach::logging`.
            let _logging = attach.logging();
            if let Err(e) = logger.lock().await.finish(pipe).await {
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
        let mut log_errors = LogErrors::default();
        let stopped = attach.output_stopped().clone();

        // When the line which waits for its ending in the log gets written as
        // a partial one, known from the last write, so that the logger is not
        // locked for it on every read. Set once the line started, a line
        // which trickles in must not wait for a pause of the output.
        let mut flush_at = None;

        loop {
            // Once stopped, nothing gets read anymore, also if more is there.
            let read = select! {
                biased;
                () = stopped.cancelled() => {
                    debug!("Stopping read loop, nothing takes the output anymore");
                    Self::forward(&mut message_tx, Message::Done).await;
                    return Ok(());
                }
                read = Self::read_chunk(&mut reader) => read,
                // The drivers decide whether they buffer lines at all.
                () = sleep_until(flush_at) => {
                    let _logging = attach.logging();
                    let res = logger.lock().await.flush(pipe).await;
                    log_errors.check(pipe, res.context("flush paused log line"));
                    flush_at = None;
                    continue;
                }
                // The sender keeps the buffer of a closed channel, see
                // `ContainerIO::drop_stdio`, a pipe which stays quiet would
                // never send to notice it. Only watched without a consumer,
                // the exec which has one ends soon after it closed.
                () = Self::closed(message_tx.as_ref()),
                    if message_tx.as_ref().is_some_and(|tx| !tx.consumed()) =>
                {
                    trace!("Stdio channel closed, dropping sender");
                    message_tx = None;
                    continue;
                }
            };

            match read {
                Ok(data) if data.is_empty() => {
                    debug!("Nothing more to read");

                    Self::forward(&mut message_tx, Message::Done).await;

                    return Ok(());
                }

                Ok(data) => {
                    trace!("Read {} bytes", data.len());

                    // Until it got broadcast below, the output is on its way,
                    // see `SharedContainerAttach::logging`.
                    let logging = log_has_drivers.then(|| attach.logging());

                    // Write to logger if drivers are configured
                    // (exec_sync uses an empty logger, so skip the overhead).
                    // A failing driver drops the chunk, the output must keep
                    // flowing to the other drivers and the attach clients.
                    if log_has_drivers {
                        let mut locked_logger = logger.lock().await;
                        let res = locked_logger.write(pipe, &data).await;
                        let pending = locked_logger.has_pending(pipe);
                        drop(locked_logger);
                        log_errors.check(pipe, res.context("write to log file"));

                        // A newline in the chunk ended the line which waited
                        // before, the one waiting now started in this chunk.
                        flush_at = match flush_at {
                            _ if !pending => None,
                            Some(at) if memchr::memchr(b'\n', &data).is_none() => Some(at),
                            _ => Some(Instant::now() + Self::IDLE_FLUSH),
                        };
                    }

                    // Bytes::clone() is just a refcount increment, no data copy
                    let attached = attach.has_readers();
                    if attached {
                        attach.write(Message::Data(data.clone(), pipe));
                    }
                    drop(logging);

                    Self::forward(&mut message_tx, Message::Data(data, pipe)).await;
                }

                // A terminal reports that the process closed it with EIO.
                Err(e) if e.raw_os_error() == Some(Errno::EIO as i32) => {
                    debug!("Stopping read loop");
                    Self::forward(&mut message_tx, Message::Done).await;

                    return Ok(());
                }
                Err(e) => return Err(e).context("read from container file descriptor"),
            }
        }
    }

    /// Read a chunk of up to [`READ_BUFFER_SIZE`] into the scratch buffer of
    /// the thread, see [`with_scratch`], and copy it out. Empty once the
    /// output ended.
    async fn read_chunk<T>(reader: &mut T) -> io::Result<Bytes>
    where
        T: AsyncRead + Unpin,
    {
        future::poll_fn(|cx| {
            with_scratch(READ_BUFFER_SIZE, |scratch| {
                let mut buf = ReadBuf::new(scratch);
                ready!(Pin::new(&mut *reader).poll_read(cx, &mut buf))?;
                Poll::Ready(Ok(Bytes::copy_from_slice(buf.filled())))
            })
        })
        .await
    }

    /// Complete once the stdio channel got closed, never without one.
    async fn closed(message_tx: Option<&StdioSender>) {
        match message_tx {
            Some(tx) => tx.tx.closed().await,
            None => future::pending().await,
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
                    if !Self::handle_stdin_data(&data, &mut writer).await? {
                        return Ok(());
                    }
                }
                () = stdin_closed.cancelled() => break,
            }
        }

        // An attach which closes stdin right after sending data may race with
        // the cancellation, so write everything that was sent before.
        attach.close_stdin();
        while let Ok(data) = attach.try_read() {
            if !Self::handle_stdin_data(&data, &mut writer).await? {
                break;
            }
        }
        Ok(())
    }

    /// Write attach input to the stdin of the process, false once that got
    /// closed. The process exited or closed its stdin, which a pipe reports
    /// as EPIPE and a terminal as EIO, which ends the stdin like its EOF.
    async fn handle_stdin_data(data: &[u8], mut writer: impl AsyncWrite + Unpin) -> Result<bool> {
        debug!("Got {} attach bytes", data.len());

        match writer.write_all(data).await {
            Ok(()) => Ok(true),
            Err(e)
                if e.kind() == ErrorKind::BrokenPipe
                    || e.raw_os_error() == Some(Errno::EIO as i32) =>
            {
                debug!("Stopping stdin, the process closed it: {e}");
                Ok(false)
            }
            Err(e) => Err(e).context("write attach stdin to stream"),
        }
    }
}

#[cfg(test)]
mod tests;
