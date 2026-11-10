//! The attach sockets themselves: accepting the clients, reading their
//! stdin and writing the output to them.

use super::{AttachListener, ListenerCleanup, SharedContainerAttach};
use crate::{
    async_fd,
    container_io::{DRAIN_MAX_WAIT, Drain, Lag, Message, READ_BUFFER_SIZE},
    fs_util::FileId,
    listener,
    mutex::lock,
    util::with_scratch,
};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
    sys::socket::{Backlog, SockType, UnixAddr, bind, listen},
};
use std::{
    fs, future,
    io::{self, IoSlice},
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::FileTypeExt,
    },
    path::Path,
    pin::Pin,
    sync::{Arc, atomic::Ordering},
    task::{Poll, ready},
};
use tokio::{
    io::{ErrorKind, Interest},
    select,
    sync::broadcast::{Receiver, error::RecvError},
    task, time,
};
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, trace, warn};

/// Check if no more input will arrive after a zero byte read. That is the
/// case once the peer closed its write side, which a SOCK_SEQPACKET socket
/// reports as POLLRDHUP, and no data is queued anymore. Checking the flag
/// first makes sure that no data arrives in between.
#[expect(unsafe_code, reason = "the FIONREAD ioctl")]
fn input_ended(socket: &UnixSeqpacket) -> io::Result<bool> {
    // Besides POLLRDHUP, poll can only report a hangup or an error, which
    // end the reading as well. The number of ready fds tells that, nix
    // does not decode the POLLRDHUP bit of the returned events.
    let events = PollFlags::from_bits_retain(libc::POLLRDHUP);
    let read_closed = loop {
        match poll(
            &mut [PollFd::new(socket.as_fd(), events)],
            PollTimeout::ZERO,
        ) {
            Err(Errno::EINTR) => {}
            res => break res? > 0,
        }
    };
    if !read_closed {
        return Ok(false);
    }

    // For a SOCK_SEQPACKET socket, FIONREAD sums up all queued packets.
    let mut queued: libc::c_int = 0;
    // SAFETY: FIONREAD writes a single c_int.
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::FIONREAD, &mut queued) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(queued == 0)
}

/// The most data of an output packet, after its pipe byte. Like for conmon,
/// CRI-O and the Go client read one byte more than this, so that a full read
/// of a pipe goes out as one packet.
pub(super) const PACKET_BUF_SIZE: usize = 8192;

// The write loop sends each read of a pipe as one packet.
const _: () = assert!(READ_BUFFER_SIZE <= PACKET_BUF_SIZE);

/// The maximum size of a stdin packet from an attach client, a larger one
/// gets truncated. The Go client writes at most 32 KiB at once, like CRI-O
/// does for conmon, plus the start of a detach key sequence which it held
/// back, a few bytes which the extra KiB leaves room for. The scratch buffer
/// of the thread keeps this size after the first stdin, see [`with_scratch`].
pub(super) const STDIN_PACKET_MAX_SIZE: usize = 33 * 1024;

/// The packet indicating that we're done writing.
pub(super) const DONE_PACKET: &[u8; 1] = &[0];

/// Listen on the attach socket at `path`, unless a listener of this server
/// does already.
pub(super) fn create_listener(
    path: &Path,
    attach: SharedContainerAttach,
    token: CancellationToken,
) -> Result<()> {
    // Held until the new listener got registered, so that a listener which
    // stops meanwhile does not remove its socket.
    let mut listeners = lock(&attach.listeners);
    if let Some(listener) = listeners.remove(path) {
        if !listener.stop.is_cancelled() && listener.is_bound_at(path) {
            debug!("Already listening on attach socket {}", path.display());
            listeners.insert(path.to_path_buf(), listener);
            return Ok(());
        }
        // Nobody can connect to a socket which got removed, for example by
        // an older client, or replaced. Stop accepting on it, otherwise
        // the listener would wait until the container exits.
        debug!(
            "Attach socket {} got removed or replaced, listening again",
            path.display()
        );
        listener.stop.cancel();
    }

    // A socket of this path which no listener of this server accepts on
    // is left over, for example by a previous server.
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => {
            debug!("Removing stale attach socket {}", path.display());
            fs::remove_file(path).context("remove stale attach socket")?;
        }
        Ok(_) => bail!(
            "attach socket path {} exists and is no socket",
            path.display()
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("check attach socket path"),
    }

    debug!("Creating attach socket: {}", path.display());
    let fd = listener::private_socket(SockType::SeqPacket).context("create attach socket")?;

    // keep parent_fd in scope until the bind, or else the socket will not work
    let (shortened_path, _parent_dir) = listener::shorten_socket_path(path)?;
    let addr = UnixAddr::new(&shortened_path).context("create socket addr")?;
    bind(fd.as_raw_fd(), &addr).context("bind socket fd")?;

    // Nothing listens on the socket if the setup fails, remove it. The
    // socket stays open until then, so the file is still the own one.
    let remove = |e: anyhow::Error| {
        if let Err(e) = fs::remove_file(path) {
            debug!("Unable to remove attach socket {}: {e:#}", path.display());
        }
        e
    };
    let held = fd
        .try_clone()
        .context("duplicate attach socket")
        .map_err(remove)?;
    let setup = || -> Result<(fs::Metadata, UnixSeqpacketListener)> {
        // Without following a symlink, like `FileId::is_at`.
        let metadata = fs::symlink_metadata(path).context("get attach socket metadata")?;
        listen(&fd, Backlog::MAXCONN).context("listen on socket fd")?;
        let listener = UnixSeqpacketListener::try_from(fd).context("create seqpacket listener")?;
        Ok((metadata, listener))
    };
    let (metadata, mut listener) = setup().map_err(remove)?;
    drop(held);

    let stop = CancellationToken::new();
    let id = AttachListener::next_id();
    listeners.insert(
        path.to_path_buf(),
        AttachListener {
            id,
            file: FileId::new(&metadata),
            stop: stop.clone(),
        },
    );
    drop(listeners);

    let cleanup = ListenerCleanup {
        attach,
        path: path.to_path_buf(),
        id,
    };
    task::spawn(
        async move {
            accept_connections(&mut listener, &cleanup.path, &cleanup.attach, token, stop).await;
            // The cleanup runs before the listener gets closed.
            drop(cleanup);
            drop(listener);
        }
        .instrument(debug_span!("attach")),
    );

    Ok(())
}

/// Accept connections until the container exits, which cancels `token`,
/// or another listener replaces this one, which cancels `stop`.
async fn accept_connections(
    listener: &mut UnixSeqpacketListener,
    path: &Path,
    attach: &SharedContainerAttach,
    token: CancellationToken,
    stop: CancellationToken,
) {
    debug!("Start listening on attach socket {}", path.display());
    let mut accept_errors = listener::AcceptErrors::default();
    loop {
        let accepted = select! {
            accepted = listener.accept() => accepted,
            () = token.cancelled() => {
                debug!("Stopping attach listener because token cancelled");
                return;
            }
            () = stop.cancelled() => {
                debug!("Stopping attach listener because it got replaced");
                return;
            }
        };
        match accepted {
            Ok(socket) => {
                debug!("Got new attach stream connection");
                let read = Arc::new(socket);
                let write = read.clone();

                let attach_clone = attach.clone();
                let token_clone = token.clone();
                task::spawn(
                    async move {
                        if let Err(e) = read_loop(read, attach_clone, token_clone).await {
                            error!("Attach read loop failure: {e:#}");
                        }
                    }
                    .instrument(debug_span!("read_loop")),
                );

                let (write_half_rx, output_ended) = attach.subscribe();
                let attach_clone = attach.clone();
                let token_clone = token.clone();
                task::spawn(
                    async move {
                        if let Err(e) = write_loop(
                            write,
                            attach_clone,
                            write_half_rx,
                            output_ended,
                            token_clone,
                        )
                        .await
                        {
                            error!("Attach write loop failure: {e:#}");
                        }
                    }
                    .instrument(debug_span!("write_loop")),
                );
            }
            Err(e) => accept_errors.failed(&e).await,
        }
    }
}

/// Forward the stdin of a client until it closes, the stdin of the
/// container got closed or the container exited, which cancels `token`.
pub(super) async fn read_loop(
    socket: Arc<UnixSeqpacket>,
    attach: SharedContainerAttach,
    token: CancellationToken,
) -> Result<()> {
    let stop_after_stdin_eof = attach.stop_after_stdin_eof.load(Ordering::Acquire);
    let stopped = async || {
        select! {
            () = token.cancelled() => debug!("Exiting because token cancelled"),
            () = attach.stdin_closed.cancelled() => debug!("Exiting because stdin got closed"),
        }
    };

    loop {
        // A client which stays connected must not keep the loop going once
        // the container exited or its stdin got closed.
        let read = select! {
            read = recv_stdin(&socket) => read,
            () = stopped() => return Ok(()),
        };

        match read {
            Ok((data, truncated)) if !data.is_empty() => {
                if truncated {
                    warn!("Truncated attach stdin packet to {} bytes", data.len());
                }
                debug!("Read {} stdin bytes from client", data.len());
                // Waiting for the container to take the data must not
                // outlive the container or its stdin.
                select! {
                    () = attach.send_stdin(data) => {}
                    () = stopped() => return Ok(()),
                }
            }
            Ok(_) => {
                // A zero byte read on a SOCK_SEQPACKET socket is either an
                // empty packet or the peer closing its write side, so
                // check the socket rather than guessing from another read.
                if !input_ended(&socket).context("check attach socket for more input")? {
                    trace!("Ignoring empty packet");
                    continue;
                }
                // The client closed its write half, no more input will
                // ever arrive on this connection. The write loop keeps
                // delivering output independently.
                debug!("Stopping read loop because there is nothing more to read");
                if stop_after_stdin_eof {
                    attach.close_container_stdin();
                }
                return Ok(());
            }
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                // The client closed the connection without reading all of
                // its output. The error gets reported only once, the
                // stdin queued before is still readable.
                debug!("Attach client reset the connection");
            }
            Err(e) => return Err(e).context("read from attach socket"),
        }
    }
}

/// Receive a stdin packet into the scratch buffer of the thread, see
/// [`with_scratch`], and copy it out, so that a connection holds no
/// buffer while it waits and the queued stdin only the data of the
/// packets. Stdin is binary, null bytes are part of the data. Returns
/// whether the packet got truncated as well.
async fn recv_stdin(socket: &UnixSeqpacket) -> io::Result<(Bytes, bool)> {
    future::poll_fn(|cx| {
        with_scratch(STDIN_PACKET_MAX_SIZE, |buf| {
            let info = ready!(socket.poll_recv(cx, buf))?;
            let data = Bytes::copy_from_slice(&buf[..info.bytes_read()]);
            Poll::Ready(Ok((data, info.truncated())))
        })
    })
    .await
}

/// Whether a send failed because the client closed its end. A client
/// which leaves output unread gets the connection reset.
fn client_gone(e: &io::Error) -> bool {
    matches!(e.kind(), ErrorKind::BrokenPipe | ErrorKind::ConnectionReset)
}

/// Forward the container output of `attach` to a client until the output
/// ended or the client went away. Once the container exited, which
/// cancels `token`, a process left behind may keep the output open, so
/// forward only what arrives shortly after the exit, see
/// [`Drain::recv_attach`].
pub(super) async fn write_loop(
    socket: Arc<UnixSeqpacket>,
    attach: SharedContainerAttach,
    mut rx: Receiver<Message>,
    output_ended: bool,
    token: CancellationToken,
) -> Result<()> {
    let hangup = hangup(&socket);
    tokio::pin!(hangup);
    let mut drain: Option<Drain> = None;
    let mut lag = Lag::default();
    loop {
        // The done message may have been sent before subscribing, so write
        // it once nothing is queued anymore.
        let message = if output_ended {
            rx.try_recv().unwrap_or(Message::Done)
        } else {
            let res = select! {
                // The exit starts the drain once, also while the output
                // keeps coming.
                biased;
                () = token.cancelled(), if drain.is_none() => {
                    debug!("Draining the output because token cancelled");
                    drain = Some(Drain::default());
                    continue;
                }
                () = &mut hangup => {
                    debug!("Exiting because the client hung up");
                    return Ok(());
                }
                res = next_message(&attach, &mut rx, drain.as_mut(), &mut lag) => res,
            };
            match res {
                Ok(message) => message,
                Err(RecvError::Lagged(n)) => {
                    lag.skipped(n);
                    continue;
                }
                Err(RecvError::Closed) => {
                    debug!("Exiting because container I/O is gone");
                    return Ok(());
                }
            }
        };
        let draining = drain.is_some();
        match message {
            Message::Done => {
                debug!("Exiting because the container output ended");
                let packet = [IoSlice::new(DONE_PACKET)];
                match send_packet(&socket, &packet, hangup.as_mut(), &token, draining).await {
                    Ok(true) => debug!("Wrote done packet to client"),
                    Ok(false) => {}
                    Err(e) => return Err(e).context("write done packet"),
                }
                return Ok(());
            }
            Message::Data(buf, pipe) => {
                // A read of a pipe fits into one packet, see
                // `PACKET_BUF_SIZE`. Send the pipe header and the data as
                // one packet without copying the data into a separate
                // buffer.
                debug_assert!(buf.len() <= PACKET_BUF_SIZE);
                let header = [pipe.attach_packet_byte()];
                let packet = [IoSlice::new(&header), IoSlice::new(&buf)];
                match send_packet(&socket, &packet, hangup.as_mut(), &token, draining).await {
                    Ok(true) => debug!("Wrote {pipe} packet to client"),
                    // The client is gone, so stop receiving output for it
                    // as well.
                    Ok(false) => return Ok(()),
                    Err(e) => return Err(e).context("write packet"),
                }
            }
        }
    }
}

/// Send a packet, false if the client went away or does not take it in
/// time. A client which does not read keeps a send waiting, until it
/// hangs up or, once the container exited, for at most
/// [`DRAIN_MAX_WAIT`]. Each packet gets that long, so that a slow client
/// still gets the output which was there already.
async fn send_packet(
    socket: &UnixSeqpacket,
    packet: &[IoSlice<'_>],
    hangup: Pin<&mut impl Future<Output = ()>>,
    token: &CancellationToken,
    draining: bool,
) -> io::Result<bool> {
    let stuck = async {
        if !draining {
            token.cancelled().await;
        }
        time::sleep(DRAIN_MAX_WAIT).await;
    };
    select! {
        res = socket.send_vectored(packet) => match res {
            Ok(_) => Ok(true),
            Err(ref e) if client_gone(e) => {
                debug!("Exiting because the client closed the connection");
                Ok(false)
            }
            Err(e) => Err(e),
        },
        () = hangup => {
            debug!("Exiting because the client hung up");
            Ok(false)
        }
        () = stuck => {
            debug!("Exiting because the client does not take the output after the exit");
            Ok(false)
        }
    }
}

/// The next output message, bounded by the drain once the container
/// exited. If the output did not end after the exit, the client gets
/// the done packet anyway.
async fn next_message(
    attach: &SharedContainerAttach,
    rx: &mut Receiver<Message>,
    drain: Option<&mut Drain>,
    lag: &mut Lag,
) -> Result<Message, RecvError> {
    match drain {
        Some(drain) => Ok(drain
            .recv_attach(attach, rx, Some(lag))
            .await
            .unwrap_or(Message::Done)),
        None => {
            let res = rx.recv().await;
            if res.is_ok() {
                lag.received(rx);
            }
            res
        }
    }
}

/// Wait until the client closed the connection. Without output, a send
/// never notices, and a client which only closed its write side still
/// receives the output. The hangup gets reported by an epoll instance of
/// its own, which watches the socket for nothing but the hangups and
/// errors epoll always reports. Neither input nor output wakes it up,
/// and its readiness is separate from the one of the socket, which the
/// read and write loops use. If that fails, only a failing send ends
/// the loop.
async fn hangup(socket: &UnixSeqpacket) {
    let wait = async || -> Result<()> {
        let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).context("create epoll")?;
        epoll
            .add(socket.as_fd(), EpollEvent::new(EpollFlags::EPOLLET, 0))
            .context("watch attach socket")?;
        let ready =
            async_fd::register(epoll.0.as_fd(), Interest::READABLE).context("register epoll")?;
        let mut events = [EpollEvent::empty()];
        loop {
            let mut guard = ready.readable().await.context("wait for epoll")?;
            let n = epoll
                .wait(&mut events, PollTimeout::ZERO)
                .context("read epoll events")?;
            if n > 0 {
                return Ok(());
            }
            guard.clear_ready();
        }
    };
    if let Err(e) = wait().await {
        warn!("Unable to watch for the attach client to hang up: {e:#}");
        future::pending::<()>().await;
    }
}
