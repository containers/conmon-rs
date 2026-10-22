//! What exec and attach sessions share: the connection to the client, its
//! input and the final status.

use super::{MAX_MESSAGE_SIZE, Session, StreamingServer};
use crate::{
    attach::SharedContainerAttach,
    container_io::SharedContainerIO,
    idle::Activity,
    remotecommand::{
        ClientEvent, ClientInput, ExpectedStreams, Framing, Output, SpdyStreams, Status, Stdin,
        Version, WebSocketInput,
    },
    spdy::{self, Connection},
    web::{self, WebSocket},
};
use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use futures::stream::StreamExt;
use serde::Deserialize;
use std::{
    collections::VecDeque,
    future::Future,
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    select,
    sync::{
        mpsc::{self, Receiver as MpscReceiver, Sender as MpscSender},
        watch,
    },
    task::{self, JoinHandle},
    time::{self, Duration},
};
use tracing::{debug, error, info, trace};

/// How much stdin the read task holds while the container does not take it,
/// on top of the stdin channel. The socket is still read meanwhile, so that
/// resize events and the close do not wait behind it. The message which
/// fills it comes on top.
pub(super) const PENDING_STDIN_MAX_BYTES: usize = MAX_MESSAGE_SIZE;

/// The largest part of stdin which goes towards the container at once, like
/// the largest packets of the attach sockets. Larger stdin gets split without
/// copying it, so that the channels on the way, which count entries, hold at
/// most this much per entry.
const STDIN_CHUNK_SIZE: usize = 64 * 1024;

/// Stdin smaller than this gets merged with the stdin before it while it
/// waits, so that a client which sends a byte per message does not make the
/// server hold far more than the bytes it counts.
const STDIN_MERGE_SIZE: usize = 32 * 1024;

/// What an entry of the pending stdin counts on top of its bytes.
const STDIN_ENTRY_OVERHEAD: usize = 64;

/// How many entries of stdin wait for the session loop. Each is at most
/// [`STDIN_CHUNK_SIZE`], more of them would only hold more stdin which the
/// container did not take.
const STDIN_CHANNEL_SIZE: usize = 1;

/// How often a session pings the client while its stdin waits for the
/// container, to notice a client which went away.
const STDIN_PING_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(5)
};

/// How often a SPDY session pings the client, to notice a client which went
/// away also while its stdin waits for the container.
const SPDY_PING_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    spdy::PING_INTERVAL
};

/// How long stdin which the client sent right before closing may take to
/// reach the container.
const STDIN_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// The latest resize event of a client, older ones are replaced.
pub(super) type ResizeRx = watch::Receiver<Option<Bytes>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
/// Terminal resize event for exec and attach.
struct ResizeEvent {
    width: u16,
    height: u16,
}

impl StreamingServer {
    /// Handle a single websocket connection.
    pub(super) async fn handle_websocket(socket: WebSocket, session: Session, framing: Framing) {
        let (sender, messages) = socket.split();
        let activity = Activity::new();
        let mut output = Output::websocket(sender, framing, activity.clone());
        // A client which went away already ends the session in its loop,
        // which tears it down.
        if let Err(e) = output.announce(session.stdout(), session.stderr()).await {
            debug!("Unable to announce the session: {e:#}");
        }
        let input = WebSocketInput {
            messages,
            framing,
            activity: activity.clone(),
        };
        Self::run_session(output, input, session, activity).await;
        info!("Closing websocket connection");
    }

    /// Handle a connection which got upgraded to SPDY: wait for the streams of
    /// the client, then run the session on them.
    pub(super) async fn handle_spdy<T>(io: T, session: Session, version: Version)
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let terminal = session.terminal().await;
        // A terminal merges stderr into stdout, Kubernetes clients do not
        // open a stream for it then.
        let expected = ExpectedStreams {
            stdin: session.stdin(),
            stdout: session.stdout(),
            stderr: session.stderr() && !terminal,
        };
        // The session ends the connection once it was idle, which reports
        // the timeout to the client first.
        let mut connection =
            Connection::serve(io, None, Some(SPDY_PING_INTERVAL), move |headers| {
                SpdyStreams::accept(headers, version)
            });
        let activity = connection.activity();
        let streams = match SpdyStreams::wait(&mut connection, expected).await {
            Ok(streams) => streams,
            Err(e) => {
                debug!("Unable to start the session: {e:#}");
                connection.abort();
                return;
            }
        };
        let (input, output) = streams.into_parts(connection, version).await;
        Self::run_session(Output::spdy(output), input, session, activity).await;
        info!("Closing SPDY connection");
    }

    /// Run a session until it ended or the client went away. `activity`
    /// tells when the connection was active the last time, for the idle
    /// timeout of the session.
    async fn run_session(
        output: Output,
        input: impl ClientInput,
        session: Session,
        activity: Activity,
    ) {
        let peer_closed = output.peer_closed();
        let (stdin_tx, stdin_rx) = mpsc::channel(STDIN_CHANNEL_SIZE);
        // Resize events go through their own channel, so that they do not
        // queue behind stdin the container does not read. Only the latest
        // size matters.
        let (resize_tx, resize_rx) = watch::channel(None);

        let mut send_task =
            Self::write_task(output, stdin_rx, resize_rx, session, activity.clone());
        let mut recv_task = Self::read_task(input, stdin_tx, resize_tx, peer_closed);

        tokio::select! {
            rv_a = (&mut send_task) => {
                let closed = match rv_a {
                    Ok(closed) => {
                        info!("All messages sent");
                        closed
                    }
                    Err(a) => {
                        error!("Error sending messages: {a:?}");
                        false
                    }
                };
                // The client answers the close of a WebSocket, the read task
                // drops what it sends until then, see
                // [`web::await_close_answer`]. A close which did not go out
                // gets no answer. A SPDY connection ended already, which ends
                // the read task.
                if closed && time::timeout(web::CLOSE_TIMEOUT, &mut recv_task).await.is_err() {
                    debug!("Client did not answer the close in time");
                }
                recv_task.abort();
            },
            rv_b = (&mut recv_task) => {
                match rv_b {
                    // After a clean close the session forwards the stdin which
                    // the client sent before, for a short time.
                    Ok(true) => {
                        info!("Client closed the connection");
                        if time::timeout(STDIN_FLUSH_TIMEOUT, &mut send_task).await.is_err() {
                            debug!("Session did not end after the close, stopping it");
                        }
                    }
                    Ok(false) => info!("Client connection ended"),
                    Err(b) => error!("Error receiving messages: {b:?}")
                }
                send_task.abort();
            }
        }
    }

    /// Build a common write task based on the session type. The loop of the
    /// session only decides its final status, which goes out after it, so
    /// that the idle timeout never cuts a status off: a client gets exactly
    /// one, or none if it went away. A session whose connection was idle for
    /// its timeout fails, like any other session which did not end, so that
    /// the client does not take it for a success, unless the process exited
    /// already. An exec process gets killed then, nothing is left to end it.
    /// The task tells whether the close went out, see [`Output::close`].
    fn write_task(
        sender: Output,
        stdin_rx: MpscReceiver<Stdin>,
        resize_rx: ResizeRx,
        session: Session,
        activity: Activity,
    ) -> JoinHandle<bool> {
        let idle_timeout = session.idle_timeout();
        tokio::spawn(async move {
            let mut sender = sender;
            let mut status = None;
            let (result, typ) = match session {
                Session::Exec(s) => (
                    async {
                        // The runtime runs in a task of its own. A client
                        // which goes away meanwhile must not kill it halfway,
                        // the process it started still gets registered, and
                        // torn down once the task finished.
                        let started = task::spawn(Self::start_exec(*s))
                            .await
                            .context("join exec start")??;
                        // The client waited for the runtime, which is no
                        // idleness of its own.
                        activity.touch();
                        let kill = started.teardown.kill.clone();
                        let exec =
                            Self::exec_loop(started, &mut sender, stdin_rx, resize_rx, &mut status);
                        Self::until_idle(exec, &activity, idle_timeout, || {
                            kill.store(true, Ordering::Release);
                        })
                        .await
                    }
                    .await,
                    "exec",
                ),
                Session::Attach(s) => (
                    Self::until_idle(
                        Self::attach_loop(s, &mut sender, stdin_rx, resize_rx, &mut status),
                        &activity,
                        idle_timeout,
                        || {},
                    )
                    .await,
                    "attach",
                ),
            };

            if let Some(status) = Self::final_status(result, status, typ)
                && let Err(e) = sender.status(&status).await
            {
                debug!("Unable to send the status: {e:#}");
            }

            sender.close().await
        })
    }

    /// The one status a client gets, out of how the loop of its session
    /// ended and the status the loop decided on, none for a client which
    /// went away.
    fn final_status(result: Result<()>, status: Option<Status>, typ: &str) -> Option<Status> {
        match result {
            Ok(()) => status,
            // The process exited before, its status tells the client.
            Err(e) if status.is_some() => {
                debug!("Sending the exit status despite: {e:#}");
                status
            }
            // Otherwise the client sees a clean close and assumes success.
            Err(e) => {
                error!("Unable to run {typ} for container: {e:#}");
                Some(Status::Error(format!("{typ} failed: {e:#}")))
            }
        }
    }

    /// Run the loop of a session until the connection was idle for the
    /// timeout, which fails it. `on_idle` runs before the loop gets dropped.
    async fn until_idle(
        run: impl Future<Output = Result<()>>,
        activity: &Activity,
        timeout: Option<Duration>,
        on_idle: impl FnOnce(),
    ) -> Result<()> {
        match activity.until_idle(timeout, run, on_idle).await {
            Some(result) => result,
            None => {
                info!("Closing idle connection");
                bail!(
                    "no activity on the connection for the idle timeout of {:?}",
                    timeout.unwrap_or_default()
                )
            }
        }
    }

    /// Build a common read task. Every message of the client it reads counts
    /// as activity, also stdin. Once stdin backs up behind a process which
    /// does not take it, the client is not read anymore and the connection
    /// may go idle, like a spdystream connection whose reader waits.
    fn read_task(
        mut input: impl ClientInput,
        stdin_tx: MpscSender<Stdin>,
        resize_tx: watch::Sender<Option<Bytes>>,
        peer_closed: Arc<AtomicBool>,
    ) -> JoinHandle<bool> {
        tokio::spawn(async move {
            let mut pending = PendingStdin::default();
            let mut closed = false;
            loop {
                select! {
                    permit = stdin_tx.reserve(), if !pending.is_empty() => match permit {
                        Ok(permit) => permit.send(pending.pop()),
                        // The session ended, nothing takes stdin anymore.
                        Err(_) => pending.clear(),
                    },
                    event = input.next(), if !pending.is_full() => match event {
                        Some(event) => {
                            if Self::read_event(event, &mut pending, &resize_tx).is_break() {
                                peer_closed.store(true, Ordering::Release);
                                closed = true;
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }

            // Stdin sent right before a clean close, like the close of the
            // stream itself, still has to reach the container. Only for a
            // short time, a container which does not read it must not delay
            // the end of the session. A client which went away gets nothing
            // forwarded anymore.
            if closed {
                let flush = async {
                    while !pending.is_empty() {
                        if stdin_tx.send(pending.pop()).await.is_err() {
                            break;
                        }
                    }
                };
                if time::timeout(STDIN_FLUSH_TIMEOUT, flush).await.is_err() {
                    debug!("Dropping stdin the container did not take after the close");
                }
            }
            closed
        })
    }

    /// Handle a single event of the client and return the control flow
    /// decision.
    fn read_event(
        event: ClientEvent,
        pending: &mut PendingStdin,
        resize_tx: &watch::Sender<Option<Bytes>>,
    ) -> ControlFlow<(), ()> {
        match event {
            ClientEvent::Stdin(stdin) => pending.push(stdin),
            ClientEvent::Resize(data) => {
                resize_tx.send_replace(Some(data));
            }
            ClientEvent::Closed => return ControlFlow::Break(()),
        }
        ControlFlow::Continue(())
    }
}

/// Forwards the stdin and the resize events of a client to the process, for
/// the loops of exec and attach sessions, which forward the output
/// meanwhile.
pub(super) struct SessionInput {
    io: SharedContainerIO,
    attach: SharedContainerAttach,
    stdin_rx: MpscReceiver<Stdin>,
    resize_rx: ResizeRx,

    /// Stdin which waits for the container.
    pending: Option<Bytes>,

    /// Pings the client while stdin waits.
    ping: time::Interval,

    /// Whether stdin gets forwarded, until the client closed it.
    pub(super) stdin_open: bool,
}

/// What the loop of a session has to handle.
#[derive(Debug, PartialEq)]
pub(super) enum InputEvent {
    /// A resize event of the client, see [`SessionInput::resize`]. Resizing
    /// does not depend on stdin, a terminal without it still has a size.
    Resize(Bytes),

    /// The stdin of the client is not read while it waits for the container,
    /// so a client which goes away meanwhile is only noticed by a failing
    /// send. The session pings it.
    Ping,

    /// The client closed its stdin.
    StdinClosed,

    /// The client closed the connection.
    Ended,
}

impl SessionInput {
    pub(super) fn new(
        io: SharedContainerIO,
        attach: SharedContainerAttach,
        stdin_rx: MpscReceiver<Stdin>,
        resize_rx: ResizeRx,
        stdin: bool,
    ) -> Self {
        let mut ping = time::interval_at(
            time::Instant::now() + STDIN_PING_INTERVAL,
            STDIN_PING_INTERVAL,
        );
        ping.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        Self {
            io,
            attach,
            stdin_rx,
            resize_rx,
            pending: None,
            ping,
            stdin_open: stdin,
        }
    }

    /// Forward stdin until the session has something to handle. The session
    /// may cancel it for its output, nothing gets lost then.
    pub(super) async fn next(&mut self) -> InputEvent {
        loop {
            select! {
                Ok(()) = self.resize_rx.changed() => {
                    if let Some(data) = self.resize_rx.borrow_and_update().clone() {
                        return InputEvent::Resize(data);
                    }
                }
                // Wait for the container to take the stdin without blocking
                // the output meanwhile.
                () = self.attach.send_stdin(self.pending.clone().unwrap_or_default()), if self.pending.is_some() => {
                    self.pending = None;
                }
                _ = self.ping.tick(), if self.pending.is_some() => return InputEvent::Ping,
                stdin = self.stdin_rx.recv(), if self.pending.is_none() => match stdin {
                    None => return InputEvent::Ended,
                    Some(_) if !self.stdin_open => {}
                    Some(Stdin::Data(data)) => self.pending = Some(data),
                    Some(Stdin::Close) => {
                        self.stdin_open = false;
                        return InputEvent::StdinClosed;
                    }
                },
            }
        }
    }

    /// Close the stdin of the container after the stdin before. A terminal
    /// cannot be closed without ending its output, it gets its end of file
    /// character instead, like from a user pressing Ctrl-D. A process
    /// reading lines sees the end of its input then, one after a line
    /// without its ending needs a second one, like a user would type it.
    pub(super) async fn close_container_stdin(&mut self) {
        match self.io.terminal_eof().await {
            Some(eof) => self.pending = Some(Bytes::from(vec![eof])),
            None => {
                self.attach.close_container_stdin();
            }
        }
    }

    /// Handle a resize event of the client, its JSON. Clients send them
    /// regardless of a terminal, a broken one does not end the session.
    pub(super) async fn resize(&self, data: &[u8]) {
        if let Err(e) = Self::try_resize(data, &self.io).await {
            debug!("Ignoring resize event: {e:#}");
        }
    }

    async fn try_resize(data: &[u8], io: &SharedContainerIO) -> Result<()> {
        let e = serde_json::from_slice::<ResizeEvent>(data).context("unmarshal resize event")?;
        trace!("Got resize message: {e:?}");
        if !io.try_resize(e.width, e.height).await? {
            debug!("Ignoring resize event, container has no terminal");
        }
        Ok(())
    }
}

/// Stdin of a client which the container did not take yet, bounded by
/// [`PENDING_STDIN_MAX_BYTES`]. Small stdin gets merged and large stdin
/// split into entries of at most [`STDIN_CHUNK_SIZE`], each counts
/// [`STDIN_ENTRY_OVERHEAD`] on top of its bytes. Empty stdin carries nothing,
/// and nothing takes stdin after the close, both get dropped. The entries
/// of large stdin keep its message until the container took the last one,
/// stdin goes to the container in order, so only the oldest message is
/// partly taken.
#[derive(Debug, Default)]
struct PendingStdin {
    /// Stdin of at least [`STDIN_MERGE_SIZE`] bytes and merged small stdin,
    /// the oldest first.
    chunks: VecDeque<Bytes>,

    /// Small stdin after the chunks, merged.
    tail: BytesMut,

    /// The bytes and the overhead of the chunks.
    bytes: usize,

    /// Whether the client closed its stdin.
    closed: bool,

    /// Whether the close still has to be taken, after the stdin before it.
    close_pending: bool,
}

impl PendingStdin {
    fn is_empty(&self) -> bool {
        self.chunks.is_empty() && self.tail.is_empty() && !self.close_pending
    }

    /// What the pending stdin counts against [`PENDING_STDIN_MAX_BYTES`].
    fn len(&self) -> usize {
        let tail = if self.tail.is_empty() {
            0
        } else {
            self.tail.len() + STDIN_ENTRY_OVERHEAD
        };
        let close = if self.close_pending {
            STDIN_ENTRY_OVERHEAD
        } else {
            0
        };
        self.bytes + tail + close
    }

    /// Whether the client has to wait, which stops reading the socket.
    fn is_full(&self) -> bool {
        self.len() >= PENDING_STDIN_MAX_BYTES
    }

    fn push(&mut self, stdin: Stdin) {
        if self.closed {
            return;
        }
        match stdin {
            Stdin::Close => {
                self.closed = true;
                self.close_pending = true;
            }
            Stdin::Data(data) if data.len() < STDIN_MERGE_SIZE => {
                if self.tail.len() + data.len() > STDIN_CHUNK_SIZE {
                    self.flush_tail();
                }
                self.tail.extend_from_slice(&data);
            }
            Stdin::Data(mut data) => {
                self.flush_tail();
                while !data.is_empty() {
                    let chunk = data.split_to(data.len().min(STDIN_CHUNK_SIZE));
                    self.push_chunk(chunk);
                }
            }
        }
    }

    /// The merged stdin becomes a chunk, so that later stdin goes behind it.
    fn flush_tail(&mut self) {
        if !self.tail.is_empty() {
            let tail = self.take_tail();
            self.push_chunk(tail);
        }
    }

    /// The merged stdin in a buffer of its own. A part of the buffer of the
    /// tail would keep all of it, which can be far more than the stdin it
    /// counts. The tail keeps its buffer, of at most twice
    /// [`STDIN_CHUNK_SIZE`], for the next stdin.
    fn take_tail(&mut self) -> Bytes {
        let tail = Bytes::copy_from_slice(&self.tail);
        self.tail.clear();
        tail
    }

    fn push_chunk(&mut self, chunk: Bytes) {
        self.bytes += chunk.len() + STDIN_ENTRY_OVERHEAD;
        self.chunks.push_back(chunk);
    }

    /// The oldest stdin, callers check that there is some.
    fn pop(&mut self) -> Stdin {
        if let Some(chunk) = self.chunks.pop_front() {
            self.bytes -= chunk.len() + STDIN_ENTRY_OVERHEAD;
            return Stdin::Data(chunk);
        }
        if !self.tail.is_empty() {
            return Stdin::Data(self.take_tail());
        }
        self.close_pending = false;
        Stdin::Close
    }

    /// Drop the stdin, nothing takes it anymore.
    fn clear(&mut self) {
        self.chunks.clear();
        self.tail.clear();
        self.bytes = 0;
        self.close_pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        container_io::{ContainerIO, Message as IOMessage, Pipe, SharedContainerIO},
        container_log::ContainerLog,
        remotecommand::{ClientEvent, STDIN_BYTE, STREAM_ERR_BYTE, Status, Stdin},
        spdy::testing::{Client, Frame},
        streaming_server::{
            AttachSession, StreamingServer, session::PENDING_STDIN_MAX_BYTES, testing::*,
        },
    };
    use anyhow::{Context, Result, anyhow, bail};
    use bytes::Bytes;

    use std::ops::ControlFlow;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::watch,
        task::LocalSet,
        time::{self, Duration},
    };

    #[tokio::test]
    async fn stdin_before_close_reaches_the_container() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut stream) = attach_session(true, false).await?;

                // More stdin than the channels on the way hold, right before
                // the close, in one write.
                let mut frames = vec![];
                for i in 0..50u8 {
                    frames.extend(client_frame(0x2, &[STDIN_BYTE, i]));
                }
                frames.extend(client_frame(0x8, &1000u16.to_be_bytes()));
                stream.write_all(&frames).await?;

                // The container writes output meanwhile, which cannot be sent
                // after the close anymore. The stdin which waits gets merged.
                let mut attach = io.attach().await;
                let mut stdin = vec![];
                while stdin.len() < 50 {
                    attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                    let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                    stdin.extend_from_slice(&data);
                }
                assert_eq!(stdin, (0..50u8).collect::<Vec<_>>());
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn idle_websocket_sessions_end() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let timeout = Duration::from_millis(500);
                let (io, _server, mut stream) =
                    attach_session_with(false, true, true, Some(timeout)).await?;
                let attach = io.attach().await;

                // Stdin keeps the session going.
                for _ in 0..20 {
                    time::sleep(timeout / 10).await;
                    stream
                        .write_all(&client_frame(0x2, &[STDIN_BYTE, b'x']))
                        .await?;
                }
                assert!(!attach.stdin_closed().is_cancelled());

                // A heartbeat of pings and pongs does not, the session ends
                // like for a client which went away, which closes the stdin
                // once and the connection.
                let pongs = async {
                    for opcode in [0x9, 0xa].into_iter().cycle() {
                        time::sleep(timeout / 5).await;
                        if stream.write_all(&client_frame(opcode, &[])).await.is_err() {
                            return;
                        }
                    }
                };
                tokio::select! {
                    result = time::timeout(4 * timeout, attach.stdin_closed().cancelled()) => result?,
                    () = pongs => bail!("connection ended before the stdin got closed"),
                }
                // The client learns about the timeout before the close, which
                // it answers.
                let mut output = vec![];
                loop {
                    match server_frame(&mut stream).await? {
                        (0x8, _) => break,
                        (_, payload) => output.extend(payload),
                    }
                }
                let output = String::from_utf8_lossy(&output);
                assert!(output.contains("idle timeout"), "{output}");
                stream.write_all(&client_frame(0x8, &[])).await?;
                let mut rest = vec![];
                time::timeout(Duration::from_secs(5), stream.read_to_end(&mut rest)).await??;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn stdin_backed_up_behind_the_container_goes_idle() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let timeout = Duration::from_millis(500);
                let (io, _server, mut stream) =
                    attach_session_with(false, true, true, Some(timeout)).await?;
                let attach = io.attach().await;

                // Stdin the container never reads, until the client is not
                // read anymore. The session ends like for a client which went
                // away, although the client still sends.
                let mut frame = vec![STDIN_BYTE];
                frame.resize(60_000, b'x');
                let frame = client_frame(0x2, &frame);
                let send = async { while stream.write_all(&frame).await.is_ok() {} };
                // The stdin gets closed before the connection ends.
                tokio::select! {
                    biased;
                    result = time::timeout(10 * timeout, attach.stdin_closed().cancelled()) => {
                        result.context("the session did not end")?;
                    }
                    () = send => bail!("connection ended before the stdin got closed"),
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn clients_which_stop_reading_go_idle() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let timeout = Duration::from_secs(1);
                let (io, _server, stream) =
                    attach_session_with(false, true, true, Some(timeout)).await?;
                let attach = io.attach().await;

                // The container writes output the client never reads.
                let output = {
                    let attach = attach.clone();
                    tokio::spawn(async move {
                        let data = Bytes::from(vec![b'o'; 64 * 1024]);
                        loop {
                            attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
                            time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                };
                let ended = time::timeout(10 * timeout, attach.stdin_closed().cancelled()).await;
                output.abort();
                drop(stream);
                ended.context("the session of a stuck client did not end")?;
                Ok(())
            })
            .await
    }

    #[test]
    fn only_one_final_status() {
        let idle = || {
            Err(anyhow!(
                "no activity on the connection for the idle timeout"
            ))
        };
        // A status the loop decided on goes out also after the idle timeout,
        // a failure never turns into a success.
        assert_eq!(
            StreamingServer::final_status(idle(), Some(Status::exited(1)), "exec"),
            Some(Status::ExitCode(1))
        );
        assert_eq!(
            StreamingServer::final_status(Ok(()), Some(Status::exited(1)), "exec"),
            Some(Status::ExitCode(1))
        );
        assert!(matches!(
            StreamingServer::final_status(idle(), None, "exec"),
            Some(Status::Error(e)) if e.contains("idle timeout")
        ));
        // A client which went away gets none.
        assert_eq!(StreamingServer::final_status(Ok(()), None, "attach"), None);
    }

    #[tokio::test]
    async fn idle_timeouts_of_stuck_websocket_sessions_send_one_status() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let timeout = Duration::from_millis(500);
                let (io, _server, mut stream) =
                    attach_session_with(false, false, true, Some(timeout)).await?;
                let attach = io.attach().await;

                // More output than the connection holds while the client does
                // not read, then the container exits, around the timeout.
                let data = Bytes::from(vec![b'o'; 64 * 1024]);
                for _ in 0..256 {
                    attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
                }
                time::sleep(timeout).await;
                attach.write(IOMessage::Done);
                time::sleep(2 * timeout).await;

                // Exactly one status, which is no success.
                let statuses = time::timeout(Duration::from_secs(10), async {
                    let mut statuses = vec![];
                    loop {
                        let (opcode, payload) = server_frame(&mut stream).await?;
                        match opcode {
                            8 => return anyhow::Ok(statuses),
                            2 if payload.first() == Some(&STREAM_ERR_BYTE) => {
                                statuses.push(String::from_utf8_lossy(&payload[1..]).into_owned());
                            }
                            _ => {}
                        }
                    }
                })
                .await??;
                assert_eq!(statuses.len(), 1, "{statuses:?}");
                assert!(!statuses[0].contains("\"Success\""), "{statuses:?}");
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn resize_without_terminal_is_ignored() -> Result<()> {
        let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
        SessionInput::try_resize(br#"{"Width":80,"Height":24}"#, &io).await?;

        // Garbage is still an error.
        assert!(SessionInput::try_resize(b"x", &io).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn spdy_stdin_stays_open_until_the_end() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut client) = spdy_attach(false).await?;
                // Client-go takes the end of the stdin stream of the server as
                // the end of stdin.
                let early = async {
                    loop {
                        if let Some(Frame::Data {
                            stream_id: 3,
                            fin: true,
                            ..
                        }) = client.frame().await?
                        {
                            return anyhow::Ok(());
                        }
                    }
                };
                assert!(
                    time::timeout(Duration::from_millis(300), early)
                        .await
                        .is_err()
                );

                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: Bytes::from_static(b"in"),
                    })
                    .await?;
                let mut attach = io.attach().await;
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(data, b"in".as_slice());
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn late_output_streams_end_instead_of_failing() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                // A container with a terminal, the client asks for stdin
                // without a terminal and opens stderr after the session
                // started, also for versions without resize streams.
                for protocol in ["v2.channel.k8s.io", "v4.channel.k8s.io"] {
                    let (io, child, _token) = attach_child(true)?;
                    let mut server = StreamingServer::default();
                    server.start_if_required().await?;
                    let url = server
                        .attach_url(AttachSession {
                            container_id: "id".into(),
                            child,
                            stdin: true,
                            stdout: true,
                            stderr: true,
                            stop_after_stdin_eof: false,
                            idle_timeout: None,
                        })
                        .await?;
                    let (response, stream) = upgrade(
                        &server,
                        &url,
                        "POST",
                        &format!(
                            "Connection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
                             X-Stream-Protocol-Version: {protocol}\r\n"
                        ),
                    )
                    .await?;
                    assert!(response.starts_with("http/1.1 101"), "{response}");
                    let mut client = Client::new(stream)?;
                    for (stream_id, typ) in [(1, "error"), (3, "stdin"), (5, "stdout")] {
                        client.open(stream_id, [("streamType", typ)].into()).await?;
                    }
                    client
                        .send(Frame::Data {
                            stream_id: 3,
                            fin: false,
                            data: Bytes::from_static(b"in"),
                        })
                        .await?;
                    let mut attach = io.attach().await;
                    let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                    assert_eq!(data, b"in".as_slice());

                    client.open(7, [("streamType", "stderr")].into()).await?;
                    let frame = client
                        .frame_until(|f| {
                            matches!(
                                f,
                                Frame::RstStream { stream_id: 7, .. }
                                    | Frame::Data {
                                        stream_id: 7,
                                        fin: true,
                                        ..
                                    }
                            )
                        })
                        .await?;
                    assert!(matches!(frame, Frame::Data { .. }), "{protocol}: {frame:?}");
                }
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn idle_spdy_sessions_end() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let timeout = Duration::from_millis(500);
                let (io, _server, mut client) =
                    spdy_attach_with(false, true, Some(timeout)).await?;
                let attach = io.attach().await;

                // Stdin keeps the session going.
                for _ in 0..20 {
                    time::sleep(timeout / 10).await;
                    client
                        .send(Frame::Data {
                            stream_id: 3,
                            fin: false,
                            data: Bytes::from_static(b"x"),
                        })
                        .await?;
                }
                assert!(!attach.stdin_closed().is_cancelled());

                // The pings of the server and their echoes do not. The client
                // learns about the timeout on the error stream, it must not
                // take the end for a success.
                let end = time::timeout(4 * timeout, async {
                    let mut status = vec![];
                    loop {
                        match client.frame().await? {
                            Some(Frame::Ping { id }) => client.send(Frame::Ping { id }).await?,
                            Some(Frame::Data {
                                stream_id: 1, data, ..
                            }) => status.extend_from_slice(&data),
                            Some(_) => {}
                            None => return anyhow::Ok(status),
                        }
                    }
                });
                let status = String::from_utf8(end.await??)?;
                assert!(status.contains("\"status\":\"Failure\""), "{status}");
                assert!(status.contains("idle timeout"), "{status}");
                assert!(attach.stdin_closed().is_cancelled());
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn idle_timeouts_of_stuck_spdy_sessions_send_one_status() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let timeout = Duration::from_millis(500);
                let (io, _server, mut client) =
                    spdy_attach_with(false, true, Some(timeout)).await?;
                let attach = io.attach().await;

                // More output than the connection holds while the client does
                // not read, then the container exits, around the timeout.
                let data = Bytes::from(vec![b'o'; 64 * 1024]);
                for _ in 0..256 {
                    attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
                }
                time::sleep(timeout).await;
                attach.write(IOMessage::Done);
                time::sleep(2 * timeout).await;

                // Exactly one status on the error stream, which is no success.
                let statuses = time::timeout(Duration::from_secs(10), async {
                    let mut statuses = vec![];
                    loop {
                        match client.frame().await? {
                            Some(Frame::Data {
                                stream_id: 1, data, ..
                            }) if !data.is_empty() => {
                                statuses.push(String::from_utf8_lossy(&data).into_owned());
                            }
                            Some(_) => {}
                            None => return anyhow::Ok(statuses),
                        }
                    }
                })
                .await??;
                assert_eq!(statuses.len(), 1, "{statuses:?}");
                assert!(statuses[0].contains("\"Failure\""), "{statuses:?}");
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn spdy_streams_after_the_start_get_refused() -> Result<()> {
        LocalSet::new()
            .run_until(async {
                let (io, _server, mut client) = spdy_attach(false).await?;
                // A second stdin stream once the session started.
                client.open(7, [("streamType", "stdin")].into()).await?;
                client
                    .frame_until(|f| matches!(f, Frame::RstStream { stream_id: 7, .. }))
                    .await?;

                // Its data does not hold up the session.
                for _ in 0..32 {
                    client
                        .send(Frame::Data {
                            stream_id: 7,
                            fin: false,
                            data: Bytes::from_static(b"late"),
                        })
                        .await?;
                }
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: Bytes::from_static(b"in"),
                    })
                    .await?;
                let mut attach = io.attach().await;
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert_eq!(data, b"in".as_slice());
                Ok(())
            })
            .await
    }

    #[test]
    fn resize_events_bypass_pending_stdin() {
        let mut pending = PendingStdin::default();
        let (resize_tx, resize_rx) = watch::channel(None);

        let mut read = |event| StreamingServer::read_event(event, &mut pending, &resize_tx);
        let stdin = || ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"a")));
        assert_eq!(read(stdin()), ControlFlow::Continue(()));
        assert_eq!(
            read(ClientEvent::Resize(Bytes::from_static(b"1"))),
            ControlFlow::Continue(())
        );
        assert_eq!(
            read(ClientEvent::Resize(Bytes::from_static(b"2"))),
            ControlFlow::Continue(())
        );

        // Only the latest size is kept, the stdin waits on its own.
        assert_eq!(resize_rx.borrow().as_deref(), Some(&b"2"[..]));
        assert_eq!(pending.pop(), Stdin::Data(Bytes::from_static(b"a")));
        assert!(pending.is_empty());

        let close = StreamingServer::read_event(ClientEvent::Closed, &mut pending, &resize_tx);
        assert_eq!(close, ControlFlow::Break(()));
    }

    #[test]
    fn pending_stdin_is_bounded() {
        let mut pending = PendingStdin::default();
        let len = PENDING_STDIN_MAX_BYTES - 64 * STDIN_ENTRY_OVERHEAD;
        pending.push(Stdin::Data(Bytes::from(vec![0; len])));
        assert_eq!(
            pending.len(),
            PENDING_STDIN_MAX_BYTES - 48 * STDIN_ENTRY_OVERHEAD
        );
        assert!(!pending.is_full());
        pending.push(Stdin::Close);
        assert!(!pending.is_full());
        // Nothing takes stdin after the close, it does not count.
        pending.push(Stdin::Data(Bytes::from(vec![0; 48 * STDIN_ENTRY_OVERHEAD])));
        assert!(!pending.is_full());

        let mut taken = 0;
        while !pending.is_empty() {
            match pending.pop() {
                Stdin::Data(data) => {
                    assert!(data.len() <= STDIN_CHUNK_SIZE);
                    taken += data.len();
                }
                Stdin::Close => assert!(pending.is_empty()),
            }
        }
        assert_eq!(taken, len);
        assert_eq!(pending.len(), 0);

        pending.clear();
        assert!(pending.is_empty());
    }

    #[test]
    fn pending_stdin_drops_what_carries_nothing() {
        let mut pending = PendingStdin::default();
        pending.push(Stdin::Data(Bytes::new()));
        assert!(pending.is_empty());

        // Only the first close counts, also without a container which takes
        // the stdin.
        for _ in 0..1000 {
            pending.push(Stdin::Close);
        }
        assert_eq!(pending.len(), STDIN_ENTRY_OVERHEAD);
        assert_eq!(pending.pop(), Stdin::Close);
        assert!(pending.is_empty());
        pending.push(Stdin::Close);
        assert!(pending.is_empty());
    }

    #[test]
    fn small_stdin_gets_merged() {
        let mut pending = PendingStdin::default();
        for i in 0..2 * STDIN_CHUNK_SIZE {
            pending.push(Stdin::Data(Bytes::from(vec![i as u8])));
        }
        // Two full entries, not one per byte.
        assert_eq!(pending.len(), 2 * (STDIN_CHUNK_SIZE + STDIN_ENTRY_OVERHEAD));
        pending.push(Stdin::Close);

        let mut data = vec![];
        while let Stdin::Data(chunk) = pending.pop() {
            data.extend_from_slice(&chunk);
        }
        assert_eq!(
            data,
            (0..2 * STDIN_CHUNK_SIZE)
                .map(|i| i as u8)
                .collect::<Vec<_>>()
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn small_stdin_does_not_keep_the_buffer_of_the_tail() {
        let mut pending = PendingStdin::default();
        pending.push(Stdin::Data(Bytes::from(vec![0; STDIN_MERGE_SIZE - 1])));
        pending.push(Stdin::Data(Bytes::from(vec![0; STDIN_MERGE_SIZE - 1])));
        // More than the tail holds, the merged stdin becomes a chunk.
        pending.push(Stdin::Data(Bytes::from_static(b"abc")));
        let Stdin::Data(merged) = pending.pop() else {
            panic!("no data");
        };
        let Stdin::Data(small) = pending.pop() else {
            panic!("no data");
        };
        assert_eq!(merged.len(), 2 * (STDIN_MERGE_SIZE - 1));
        assert_eq!(small, Bytes::from_static(b"abc"));
        // The stdin owns all of its buffer, see `Bytes::try_into_mut`.
        for data in [merged, small] {
            let len = data.len();
            assert!(data.try_into_mut().is_ok_and(|data| data.capacity() == len));
        }
    }

    #[test]
    fn large_stdin_gets_split_without_copying() {
        let mut pending = PendingStdin::default();
        pending.push(Stdin::Data(Bytes::from_static(b"a")));
        let data = Bytes::from(vec![1; 2 * STDIN_CHUNK_SIZE + 1]);
        pending.push(Stdin::Data(data.clone()));

        // The small stdin before it keeps its place.
        assert_eq!(pending.pop(), Stdin::Data(Bytes::from_static(b"a")));
        let mut offset = 0;
        for len in [STDIN_CHUNK_SIZE, STDIN_CHUNK_SIZE, 1] {
            let Stdin::Data(chunk) = pending.pop() else {
                panic!("no data");
            };
            assert_eq!(chunk.len(), len);
            assert_eq!(chunk.as_ptr(), data[offset..].as_ptr());
            offset += len;
        }
        assert!(pending.is_empty());
    }
}
