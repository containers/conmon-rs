//! What exec and attach sessions share: the connection to the client, its
//! input and the final status.

use super::{Session, StreamingServer};
use crate::{
    attach::SharedContainerAttach,
    container_io::SharedContainerIO,
    streaming_server::{
        idle::Activity,
        remotecommand::{
            ClientEvent, ClientInput, ExpectedStreams, Framing, MESSAGE_TOO_LARGE, Operation,
            Output, SpdyStreams, Status, Stdin, Version, WebSocketInput,
        },
        spdy::{self, Connection},
        web::{self, WebSocket},
    },
};
use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use futures::stream::StreamExt;
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, Visitor},
};
use std::{
    collections::VecDeque,
    fmt,
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
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace};

/// How much stdin the read task holds while the container does not take it,
/// on top of the stdin channel. The socket is still read meanwhile, so that
/// resize events and the close do not wait behind it. The message which
/// fills it comes on top, also when it is larger than all of it.
pub(super) const PENDING_STDIN_MAX_BYTES: usize = 1024 * 1024;

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

/// How often a session pings the client, to notice a client which went away
/// also while its stdin waits for the container. A WebSocket session pings
/// only while stdin waits, a SPDY connection all the time.
pub(super) const PING_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    spdy::PING_INTERVAL
};

/// How long stdin which the client sent right before closing may take to
/// reach the container.
const STDIN_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// The latest resize event of a client, older ones are replaced.
type ResizeRx = watch::Receiver<Option<Bytes>>;

/// How the input of a client ended.
enum InputEnd<I> {
    /// The client closed the connection cleanly.
    Closed,

    /// The client went away.
    Gone,

    /// The client sent a message larger than the server takes, its input
    /// still has to drain the connection, see [`ClientInput::drain`].
    TooLarge(I),
}

/// How a single event of a client ended its input, see
/// [`StreamingServer::read_event`].
#[derive(Debug, PartialEq)]
enum EventEnd {
    Closed,
    TooLarge,
}

/// What the input of a client hands to the loop of its session.
pub(super) struct ClientRx {
    stdin: MpscReceiver<Stdin>,
    resize: ResizeRx,

    /// Cancelled once the client sent a message larger than the server
    /// takes, which fails the session.
    too_large: CancellationToken,
}

#[cfg(test)]
impl ClientRx {
    /// The input of a client which sends nothing while the returned senders
    /// live.
    pub(super) fn idle() -> (Self, (MpscSender<Stdin>, watch::Sender<Option<Bytes>>)) {
        let (stdin_tx, stdin) = mpsc::channel(STDIN_CHANNEL_SIZE);
        let (resize_tx, resize) = watch::channel(None);
        let client = Self {
            stdin,
            resize,
            too_large: CancellationToken::new(),
        };
        (client, (stdin_tx, resize_tx))
    }
}

/// Terminal resize event for exec and attach. Kubernetes decodes it with
/// Go, which matches the names of the fields regardless of their case, so
/// clients send `{"width":80}` as well as `{"Width":80}`, and leaves a
/// missing or null field at zero, and a null event all zero. For `Width` and
/// `Height`, ASCII case folding accepts the same names as the Unicode case
/// folding of Go, which only adds the Kelvin sign for `k` and the long s for
/// `s`.
#[derive(Debug, Default, PartialEq)]
struct ResizeEvent {
    width: u16,
    height: u16,
}

impl ResizeEvent {
    fn parse(data: &[u8]) -> Result<Self> {
        let event: Option<Self> = serde_json::from_slice(data).context("unmarshal resize event")?;
        Ok(event.unwrap_or_default())
    }
}

impl<'de> Deserialize<'de> for ResizeEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The fields in the order of the document, like Go, where the last
        /// one of the names which match wins.
        struct Fields;

        impl<'de> Visitor<'de> for Fields {
            type Value = ResizeEvent;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a terminal size")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ResizeEvent, A::Error> {
                let mut event = ResizeEvent::default();
                while let Some(name) = map.next_key::<String>()? {
                    let field = if name.eq_ignore_ascii_case("width") {
                        &mut event.width
                    } else if name.eq_ignore_ascii_case("height") {
                        &mut event.height
                    } else {
                        map.next_value::<IgnoredAny>()?;
                        continue;
                    };
                    if let Some(value) = map.next_value()? {
                        *field = value;
                    }
                }
                Ok(event)
            }
        }

        deserializer.deserialize_map(Fields)
    }
}

impl StreamingServer {
    /// Handle a single websocket connection.
    pub(super) async fn handle_websocket(socket: WebSocket, session: Session, framing: Framing) {
        let discard = socket.get_ref().discarder();
        let written = socket.get_ref().written();
        let (sender, messages) = socket.split();
        let activity = Activity::new();
        let mut output = Output::websocket(
            sender,
            framing,
            activity.clone(),
            written,
            session.idle_timeout(),
        );
        // A client which went away already ends the session in its loop,
        // which tears it down.
        if let Err(e) = output.announce(session.stdout(), session.stderr()).await {
            debug!("Unable to announce the session: {e:#}");
        }
        let input = WebSocketInput {
            messages,
            framing,
            activity: activity.clone(),
            discard,
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
        let mut connection = Connection::serve(io, None, Some(PING_INTERVAL), move |headers| {
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
        let output = Output::spdy(output, session.idle_timeout());
        Self::run_session(output, input, session, activity).await;
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
        let too_large = CancellationToken::new();
        let client = ClientRx {
            stdin: stdin_rx,
            resize: resize_rx,
            too_large: too_large.clone(),
        };

        let mut send_task = Self::write_task(output, client, session, activity.clone());
        let mut recv_task = Self::read_task(input, stdin_tx, resize_tx, peer_closed, too_large);

        tokio::select! {
            rv_a = (&mut send_task) => {
                let output = match rv_a {
                    Ok(output) => {
                        info!("All messages sent");
                        output
                    }
                    Err(a) => {
                        error!("Error sending messages: {a:?}");
                        None
                    }
                };
                // The client answers the close of a WebSocket, the read task
                // drops what it sends until then, see
                // [`web::await_close_answer`]. A close which did not go out
                // gets no answer. A SPDY connection ended already, which ends
                // the read task. A client which sent a message which was too
                // large, also one whose session failed for it, cannot answer
                // anymore, the connection gets drained instead.
                if let Some(output) = output {
                    match time::timeout(web::CLOSE_TIMEOUT, &mut recv_task).await {
                        Ok(Ok(InputEnd::TooLarge(input))) => input.drain(output).await,
                        Ok(_) => {}
                        Err(_) => debug!("Client did not answer the close in time"),
                    }
                }
                recv_task.abort();
            },
            rv_b = (&mut recv_task) => {
                match rv_b {
                    // After a clean close the session forwards the stdin which
                    // the client sent before, for a short time.
                    Ok(InputEnd::Closed) => {
                        info!("Client closed the connection cleanly, or after its stdin ended");
                        if time::timeout(STDIN_FLUSH_TIMEOUT, &mut send_task).await.is_err() {
                            debug!("Session did not end after the close, stopping it");
                        }
                    }
                    // The session fails, the client still gets its status and
                    // the close, which tells why, and what it still sends
                    // gets drained, so that they do not get lost. A client
                    // still busy with the rest of its message reads them only
                    // once it got rid of it, see [`web::Discard`]. The loop
                    // of the session ends right away, the status and the
                    // close end once the client stopped reading, see
                    // [`Output::close`].
                    Ok(InputEnd::TooLarge(input)) => {
                        info!("Client sent a message larger than the limit");
                        if let Ok(Some(output)) = (&mut send_task).await {
                            input.drain(output).await;
                        }
                    }
                    Ok(InputEnd::Gone) => info!("Client connection ended"),
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
    /// The task returns the output once the close went out, see
    /// [`Output::close`].
    fn write_task(
        sender: Output,
        client: ClientRx,
        session: Session,
        activity: Activity,
    ) -> JoinHandle<Option<Output>> {
        let idle_timeout = session.idle_timeout();
        let too_large = client.too_large.clone();
        tokio::spawn(async move {
            let mut sender = sender;
            let mut status = None;
            let (result, operation) = match session {
                Session::Exec(s) => (
                    async {
                        // The runtime runs in a task of its own. A client
                        // which goes away meanwhile must not kill it halfway,
                        // the process it started still gets registered, and
                        // torn down once the task finished. A client which
                        // sent too much fails the session meanwhile, it
                        // does not wait for its status.
                        let start = task::spawn(Self::start_exec(*s));
                        let started = select! {
                            started = start => started.context("join exec start")??,
                            () = too_large.cancelled() => bail!(MESSAGE_TOO_LARGE),
                        };
                        // The client waited for the runtime, which is no
                        // idleness of its own.
                        activity.touch();
                        let kill = started.teardown.kill.clone();
                        let exec = Self::exec_loop(started, &mut sender, client, &mut status);
                        let exec = Self::until_too_large(exec, &too_large);
                        Self::until_idle(exec, &activity, idle_timeout, || {
                            kill.store(true, Ordering::Release);
                        })
                        .await
                    }
                    .await,
                    Operation::Exec,
                ),
                Session::Attach(s) => (
                    Self::until_idle(
                        Self::until_too_large(
                            Self::attach_loop(s, &mut sender, client, &mut status),
                            &too_large,
                        ),
                        &activity,
                        idle_timeout,
                        || {},
                    )
                    .await,
                    Operation::Attach,
                ),
            };

            // The close tells clients why, also ones which do not read the
            // status. Their session failed, its end does not wait for the
            // idle timeout, see [`Output::close_too_large`].
            if too_large.is_cancelled() {
                sender.close_too_large();
            }
            if let Some(status) = Self::final_status(result, status, operation)
                && let Err(e) = sender.status(&status).await
            {
                debug!("Unable to send the status: {e:#}");
            }

            // Also for a message which came while the status went out.
            if too_large.is_cancelled() {
                sender.close_too_large();
            }
            sender.close().await.then_some(sender)
        })
    }

    /// The one status a client gets, out of how the loop of its session
    /// ended and the status the loop decided on, none for a client which
    /// went away.
    fn final_status(
        result: Result<()>,
        status: Option<Status>,
        operation: Operation,
    ) -> Option<Status> {
        match result {
            Ok(()) => status,
            // The process exited before, its status tells the client.
            Err(e) if status.is_some() => {
                debug!("Sending the exit status despite: {e:#}");
                status
            }
            // Otherwise the client sees a clean close and assumes success.
            Err(e) => {
                error!("Unable to run {operation} for container: {e:#}");
                Some(Status::failed(operation, format_args!("{e:#}")))
            }
        }
    }

    /// Run the loop of a session until the client sent a message larger
    /// than the server takes, which fails it. The loop may wait for a client
    /// which does not read its output then, it gets dropped, so that the
    /// status and the close go out right away.
    async fn until_too_large(
        run: impl Future<Output = Result<()>>,
        too_large: &CancellationToken,
    ) -> Result<()> {
        select! {
            result = run => result,
            () = too_large.cancelled() => bail!(MESSAGE_TOO_LARGE),
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
        if let Some(result) = activity.until_idle(timeout, run, on_idle).await {
            result
        } else {
            info!("Closing idle connection");
            bail!(
                "no activity on the connection for the idle timeout of {:?}",
                timeout.unwrap_or_default()
            )
        }
    }

    /// Build a common read task. Every message of the client it reads counts
    /// as activity, also stdin. Once stdin backs up behind a process which
    /// does not take it, the client is not read anymore and the connection
    /// may go idle, like a spdystream connection whose reader waits. Its
    /// pings get no answer meanwhile either, a WebSocket client which fails
    /// without pongs, like the one of client-go after about a minute, ends
    /// the session once the process does not take stdin for that long. The
    /// pings sit behind the stdin in the socket, reading them would mean
    /// buffering the stdin without a bound.
    fn read_task<I>(
        mut input: I,
        stdin_tx: MpscSender<Stdin>,
        resize_tx: watch::Sender<Option<Bytes>>,
        peer_closed: Arc<AtomicBool>,
        too_large: CancellationToken,
    ) -> JoinHandle<InputEnd<I>>
    where
        I: ClientInput,
    {
        tokio::spawn(async move {
            let mut pending = PendingStdin::default();
            let end = loop {
                select! {
                    permit = stdin_tx.reserve(), if !pending.is_empty() => match permit {
                        Ok(permit) => permit.send(pending.pop()),
                        // The session ended, nothing takes stdin anymore.
                        Err(_) => pending.clear(),
                    },
                    // Once stdin got closed, nothing gets added anymore, so
                    // keep reading for the end of the connection.
                    event = input.next(), if !pending.is_full() || pending.closed => match event {
                        Some(event) => {
                            if let ControlFlow::Break(end) =
                                Self::read_event(event, &mut pending, &resize_tx)
                            {
                                break Some(end);
                            }
                        }
                        None => break None,
                    },
                }
            };
            match end {
                Some(EventEnd::Closed) => peer_closed.store(true, Ordering::Release),
                Some(EventEnd::TooLarge) => too_large.cancel(),
                None => {}
            }

            // Stdin sent right before a clean close, like the close of the
            // stream itself, still has to reach the container. Only for a
            // short time, a container which does not read it must not delay
            // the end of the session. A client which went away or failed the
            // session gets nothing forwarded anymore.
            if end == Some(EventEnd::Closed) {
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
            match end {
                Some(EventEnd::Closed) => InputEnd::Closed,
                Some(EventEnd::TooLarge) => InputEnd::TooLarge(input),
                None => InputEnd::Gone,
            }
        })
    }

    /// Handle a single event of the client, break once its input ended.
    fn read_event(
        event: ClientEvent,
        pending: &mut PendingStdin,
        resize_tx: &watch::Sender<Option<Bytes>>,
    ) -> ControlFlow<EventEnd> {
        match event {
            ClientEvent::Stdin(stdin) => pending.push(stdin),
            ClientEvent::Resize(data) => {
                resize_tx.send_replace(Some(data));
            }
            ClientEvent::Closed => return ControlFlow::Break(EventEnd::Closed),
            ClientEvent::TooLarge => return ControlFlow::Break(EventEnd::TooLarge),
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
    client: ClientRx,

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

    /// The client sent a message larger than the server takes, which fails
    /// the session.
    TooLarge,
}

impl SessionInput {
    pub(super) fn new(
        io: SharedContainerIO,
        attach: SharedContainerAttach,
        client: ClientRx,
        stdin: bool,
    ) -> Self {
        let mut ping = time::interval_at(time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
        ping.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        Self {
            io,
            attach,
            client,
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
                () = self.client.too_large.cancelled() => return InputEvent::TooLarge,
                Ok(()) = self.client.resize.changed() => {
                    if let Some(data) = self.client.resize.borrow_and_update().clone() {
                        return InputEvent::Resize(data);
                    }
                }
                // Wait for the container to take the stdin without blocking
                // the output meanwhile.
                () = self.attach.send_stdin(self.pending.clone().unwrap_or_default()), if self.pending.is_some() => {
                    self.pending = None;
                }
                _ = self.ping.tick(), if self.pending.is_some() => return InputEvent::Ping,
                stdin = self.client.stdin.recv(), if self.pending.is_none() => match stdin {
                    // The input fails the session before it ends.
                    None if self.client.too_large.is_cancelled() => return InputEvent::TooLarge,
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
        let e = ResizeEvent::parse(data)?;
        trace!("Got resize message: {e:?}");
        // Like CRI-O, a terminal never gets a size without rows or columns,
        // which clients send before they know their own.
        if e.width == 0 || e.height == 0 {
            debug!("Ignoring resize event without a size: {e:?}");
            return Ok(());
        }
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

/// The entry points of the fuzz targets.
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzzing;

#[cfg(test)]
mod tests;
