//! The read loops of the output and the stdin of the process.

use super::{ContainerIO, Message, Pipe, StdioSender};
use crate::{
    attach::{AttachOutput, SharedContainerAttach},
    container_log::SharedContainerLog,
    util::sleep_until,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use nix::errno::Errno;
use std::{
    cell::RefCell,
    future,
    io::{self, ErrorKind},
    pin::Pin,
    sync::Arc,
    task::{Poll, ready},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf},
    process::{ChildStderr, ChildStdin, ChildStdout},
    select, task,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, info, trace};

/// The most a read of a container I/O stream takes at once. The read loops
/// read into the scratch buffer of their thread and copy each chunk out of
/// it, see [`with_scratch`], so an idle container holds no read buffer. 8 KiB
/// halves the reads and log writes of 4 KiB for bulk output, the log files
/// start the buffer of each chunk with as much, see `LogFile::BUF_CAPACITY`.
pub(crate) const READ_BUFFER_SIZE: usize = 8 * 1024;

thread_local! {
    /// See [`with_scratch`].
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` on a buffer of `len` bytes which the thread keeps for all reads
/// which copy what they got out of it right away. The read loops of all
/// containers and attach clients and the runtime calls share it, instead of
/// keeping a buffer each while they wait. It grows to the largest `len`
/// asked for and holds what the last use left in it.
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

/// Logs a failing log driver once until it works again. A full disk would
/// otherwise log an error for every chunk of output.
#[derive(Debug, Default)]
pub(super) struct LogErrors {
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

impl ContainerIO {
    /// Read the pipes of the runtime, which the process it starts shares.
    pub(crate) fn connect_streams(
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
    pub(crate) async fn connect_terminal(
        &mut self,
        stdin: bool,
        token: CancellationToken,
    ) -> Result<()> {
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
        // A terminal reports EIO once no process has it open, which ends its
        // output. The server keeps it open itself until the exit, so that a
        // process may close and open it again, like an init system or a getty
        // does. Its output ends once the server drops it at the exit.
        match tty.open_peer() {
            Ok(peer) => {
                let exited = token.clone();
                task::spawn(async move {
                    exited.cancelled().await;
                    drop(peer);
                });
            }
            Err(e) => debug!("Unable to keep the terminal open until the exit: {e}"),
        }
        let output = self.attach.output();
        self.spawn_read_loop(tty.clone(), Pipe::StdOut, output);
        self.spawn_stdin_loop(stdin.then_some(tty), token);
        Ok(())
    }

    /// Spawn the [`ContainerIO::read_loop`] of a pipe, which takes the
    /// sender of its stdio channel.
    pub(super) fn spawn_read_loop<T>(&mut self, reader: T, pipe: Pipe, output: Arc<AttachOutput>)
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
    pub(super) const IDLE_FLUSH: Duration = Duration::from_millis(500);

    /// Read the pipe until it ends and let the log drivers write what they
    /// still buffer, however the loop ended.
    pub(crate) async fn read_loop<T>(
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

                    // Bytes::clone() is just a refcount increment, no data
                    // copy. Without clients the send fails cheaply, a check
                    // before it would miss a client which subscribes in
                    // between.
                    attach.write(Message::Data(data.clone(), pipe));
                    drop(logging);

                    Self::forward(&mut message_tx, Message::Data(data, pipe)).await;
                }

                // A terminal reports EIO once nothing has it open: at the
                // exit, or once the process closed it if the server could not
                // keep it open, see `connect_terminal`.
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

    /// Write the stdin of the attach endpoints to `writer` until they closed
    /// it or the process exited, which cancels `token`.
    pub(crate) async fn read_loop_stdin(
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
    /// closed, which ends the stdin like its EOF. A pipe reports EPIPE once
    /// the process exited or closed its stdin. A terminal reports EIO only
    /// once nothing has it open, see `connect_terminal`, otherwise its writes
    /// block once its buffer is full, until the exit ends the stdin, see
    /// `read_loop_stdin`.
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
