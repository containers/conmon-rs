//! The stdio channels and the collector of the output of an exec.

use super::{ContainerIO, Drain, Message};
use crate::{
    channel::{Receiver, RecvError, Sender},
    mutex::lock,
    tail::Tail,
    util::with_stderr,
};
use anyhow::{Result, bail};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    select,
    task::{self, JoinHandle},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

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
    pub(super) tx: Sender<Message>,
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
    pub(super) fn consumed(&self) -> bool {
        self.consumed.load(Ordering::Acquire)
    }

    /// Send a message, false if the channel got closed or all receivers are
    /// gone.
    pub(super) async fn send(&self, message: Message) -> bool {
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

/// A stdio channel of a pipe, see [`StdioSender`].
#[derive(Debug)]
pub(super) struct StdioChannel {
    /// Until the channel got closed, consumers read clones of it.
    rx: Option<Receiver<Message>>,

    /// Until the read loop of the pipe took it, once the process got
    /// connected.
    pub(super) tx: Option<StdioSender>,
}

impl StdioChannel {
    pub(super) fn new(capacity: usize, consumed: &Arc<AtomicBool>) -> Self {
        let (tx, rx) = crate::channel::bounded(capacity);
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
    pub(super) tail: Tail<{ EarlyStream::TAIL_SIZE }>,
}

impl EarlyStream {
    pub(super) const TAIL_SIZE: usize = 64 * 1024;

    pub(super) fn push(&mut self, data: &[u8]) {
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
    pub(super) fn end(&self) -> Vec<u8> {
        let tail = if self.truncated { self.tail.end() } else { &[] };
        let from_data = Self::TAIL_SIZE.saturating_sub(tail.len());
        let data = &self.data[self.data.len().saturating_sub(from_data)..];
        [data, tail].concat()
    }
}

#[derive(Debug)]
pub(super) struct StdioCollector {
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
    pub(super) async fn collect(
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
    ///
    /// [`SharedContainerAttach::stop_output`]: crate::attach::SharedContainerAttach::stop_output
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
    pub(super) async fn read_stream(
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
}
