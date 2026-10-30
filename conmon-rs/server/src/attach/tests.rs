use super::*;
use crate::{
    container_io::{ContainerIO, StdioSender},
    container_log::ContainerLog,
    test_util::wait_for,
};
use nix::sys::socket::{AddressFamily, MsgFlags, SockFlag, connect, send, socket};
use std::os::{
    fd::OwnedFd,
    unix::fs::{MetadataExt, PermissionsExt},
};
use std::{
    io,
    net::Shutdown,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    task::JoinHandle,
    time::{Duration, timeout},
};

/// A connected pair of packet sockets, like the attach clients use. The
/// first one is the server side.
fn seqpacket_pair() -> Result<(Arc<UnixSeqpacket>, UnixSeqpacket)> {
    let (server, client) = UnixSeqpacket::pair()?;
    Ok((Arc::new(server), client))
}

/// Spawn a read loop for the server side of a packet socket pair and
/// return the client side.
fn spawn_read_loop(
    attach: &SharedContainerAttach,
    token: &CancellationToken,
) -> Result<(UnixSeqpacket, JoinHandle<Result<()>>)> {
    let (server, client) = seqpacket_pair()?;
    let task = task::spawn(Attach::read_loop(server, attach.clone(), token.clone()));
    Ok((client, task))
}

struct BadFd;

impl AsyncRead for BadFd {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut TaskContext<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::from_raw_os_error(Errno::EBADF as i32)))
    }
}

#[tokio::test]
async fn output_stops_once_consumed_and_exited() -> Result<()> {
    // The consumer is done before the exit, like one which went away.
    let attach = SharedContainerAttach::default();
    let exited = CancellationToken::new();
    drop(attach.stop_output_on_exit(exited.clone()));
    task::yield_now().await;
    assert!(!attach.output_stopped().is_cancelled());
    exited.cancel();
    tokio::time::timeout(Duration::from_secs(5), attach.output_stopped().cancelled()).await?;

    // The consumer is done after the exit.
    let attach = SharedContainerAttach::default();
    let exited = CancellationToken::new();
    exited.cancel();
    drop(attach.stop_output_on_exit(exited));
    assert!(attach.output_stopped().is_cancelled());

    // The exit got lost, the output got stopped otherwise, nothing waits
    // for the exit anymore.
    let attach = SharedContainerAttach::default();
    drop(attach.stop_output_on_exit(CancellationToken::new()));
    task::yield_now().await;
    assert_eq!(Arc::strong_count(&attach.output), 2);
    attach.stop_output();
    tokio::time::timeout(Duration::from_secs(5), async {
        while Arc::strong_count(&attach.output) > 1 {
            task::yield_now().await;
        }
    })
    .await?;

    // Nothing waits if the output got stopped already.
    drop(attach.stop_output_on_exit(CancellationToken::new()));
    task::yield_now().await;
    assert_eq!(Arc::strong_count(&attach.output), 1);
    Ok(())
}

#[tokio::test]
async fn done_after_all_pipes() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let (mut rx, _) = attach.subscribe();

    let stdout = attach.output();
    let stderr = stdout.clone();

    drop(stderr);
    attach.write(Message::Data(Bytes::from_static(b"last"), Pipe::StdOut));
    drop(stdout);

    assert!(matches!(rx.recv().await?, Message::Data(..)));
    assert_eq!(rx.recv().await?, Message::Done);
    assert!(rx.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn done_after_failed_read_loop() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let (mut rx, _) = attach.subscribe();
    let (message_tx, _message_rx) = async_channel::unbounded();

    let stdout = attach.output();
    let stderr = stdout.clone();

    let res = ContainerIO::read_loop(
        BadFd,
        Pipe::StdErr,
        ContainerLog::new(),
        Some(StdioSender::new(message_tx.clone(), &Arc::default())),
        attach.clone(),
        stderr,
    )
    .await;
    assert!(res.is_err());
    assert!(rx.try_recv().is_err());

    ContainerIO::read_loop(
        tokio::io::empty(),
        Pipe::StdOut,
        ContainerLog::new(),
        Some(StdioSender::new(message_tx, &Arc::default())),
        attach,
        stdout,
    )
    .await?;

    assert_eq!(rx.recv().await?, Message::Done);
    assert!(rx.try_recv().is_err());
    Ok(())
}

/// An attach client which subscribes while the output ends either gets
/// the done message or learns that the output ended.
#[test]
fn subscribe_racing_the_end_sees_it() -> Result<()> {
    for _ in 0..2000 {
        let attach = SharedContainerAttach::default();
        let output = attach.output();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let subscriber = std::thread::spawn({
            let (attach, barrier) = (attach.clone(), barrier.clone());
            move || {
                barrier.wait();
                attach.subscribe()
            }
        });
        barrier.wait();
        drop(output);
        let (mut rx, ended) = subscriber
            .join()
            .map_err(|_| anyhow::format_err!("subscriber panicked"))?;
        assert!(ended || rx.try_recv() == Ok(Message::Done));
    }
    Ok(())
}

#[tokio::test]
async fn done_after_output_ended() -> Result<()> {
    let attach = SharedContainerAttach::default();
    drop(attach.output());

    let (server, client) = seqpacket_pair()?;
    let (rx, output_ended) = attach.subscribe();
    assert!(output_ended);
    timeout(
        Duration::from_secs(5),
        Attach::write_loop(server, rx, output_ended, CancellationToken::new()),
    )
    .await??;

    let mut buf = [1];
    assert_eq!(client.recv(&mut buf).await?.bytes_read(), 1);
    assert_eq!(&buf, Attach::DONE_PACKET);
    Ok(())
}

#[tokio::test]
async fn write_loop_stops_on_broken_pipe() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let (server, client) = seqpacket_pair()?;
    drop(client);

    let (rx, _) = attach.subscribe();
    let mut write_loop = task::spawn(Attach::write_loop(
        server,
        rx,
        false,
        CancellationToken::new(),
    ));
    // A process forked by a parallel test may hold the client end until
    // it execs, a send succeeds then. Keep sending until one fails.
    for _ in 0..500 {
        attach.write(Message::Data(Bytes::from_static(b"data"), Pipe::StdOut));
        if let Ok(res) = timeout(Duration::from_millis(10), &mut write_loop).await {
            return res?;
        }
    }
    bail!("write loop did not stop")
}

/// Receive a packet of the attach protocol, which fits what CRI-O and the
/// Go client read.
async fn recv_packet(client: &UnixSeqpacket) -> Result<Vec<u8>> {
    let mut buf = vec![0; 2 * Attach::PACKET_BUF_SIZE];
    let n = timeout(Duration::from_secs(5), client.recv(&mut buf))
        .await??
        .bytes_read();
    assert!(n <= Attach::PACKET_BUF_SIZE + 1, "packet of {n} bytes");
    buf.truncate(n);
    Ok(buf)
}

#[tokio::test]
async fn write_loop_sends_full_reads_in_one_packet() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let (server, client) = seqpacket_pair()?;
    let (rx, _) = attach.subscribe();
    let _write_loop = task::spawn(Attach::write_loop(
        server,
        rx,
        false,
        CancellationToken::new(),
    ));

    // A full read of a pipe, a larger chunk gets split.
    let full = vec![b'a'; Attach::PACKET_BUF_SIZE];
    attach.write(Message::Data(Bytes::from(full.clone()), Pipe::StdOut));
    assert_eq!(recv_packet(&client).await?, [b"\x02", &full[..]].concat());
    let larger = [&full[..], b"b"].concat();
    attach.write(Message::Data(Bytes::from(larger), Pipe::StdErr));
    assert_eq!(recv_packet(&client).await?, [b"\x03", &full[..]].concat());
    assert_eq!(recv_packet(&client).await?, b"\x03b");
    Ok(())
}

#[tokio::test]
async fn write_loop_stops_on_hangup() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let (server, client) = seqpacket_pair()?;
    let (rx, _) = attach.subscribe();
    let write_loop = task::spawn(Attach::write_loop(
        server.clone(),
        rx,
        false,
        CancellationToken::new(),
    ));

    // Closing only the write side keeps the output flowing.
    client.shutdown(Shutdown::Write)?;
    attach.write(Message::Data(Bytes::from_static(b"data"), Pipe::StdOut));
    assert_eq!(recv_packet(&client).await?, b"\x02data");
    assert!(!write_loop.is_finished());

    // A process forked by a parallel test may hold the client end until
    // it execs, which delays the hangup.
    drop(client);
    timeout(Duration::from_secs(5), write_loop).await???;
    assert_eq!(attach.receiver_count(), 0);
    Ok(())
}

#[tokio::test]
async fn write_loop_drains_after_exit() -> Result<()> {
    let attach = SharedContainerAttach::default();
    // Output which never ends, like one held open by a process the
    // container left behind.
    let _output = attach.output();
    let (server, client) = seqpacket_pair()?;
    let (rx, _) = attach.subscribe();
    let token = CancellationToken::new();
    let write_loop = task::spawn(Attach::write_loop(server, rx, false, token.clone()));

    attach.write(Message::Data(Bytes::from_static(b"out"), Pipe::StdOut));
    token.cancel();
    attach.write(Message::Data(Bytes::from_static(b"err"), Pipe::StdErr));

    assert_eq!(recv_packet(&client).await?, b"\x02out");
    assert_eq!(recv_packet(&client).await?, b"\x03err");
    assert_eq!(recv_packet(&client).await?, Attach::DONE_PACKET);
    timeout(Duration::from_secs(5), write_loop).await???;
    Ok(())
}

#[tokio::test]
async fn write_loop_gives_up_on_stuck_client_after_exit() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let _output = attach.output();
    let (server, _client) = seqpacket_pair()?;
    let (rx, _) = attach.subscribe();
    let token = CancellationToken::new();
    let write_loop = task::spawn(Attach::write_loop(server, rx, false, token.clone()));

    // A client which does not read, until its socket takes no more.
    let data = Bytes::from(vec![0; Attach::PACKET_BUF_SIZE]);
    for _ in 0..OUTPUT_CHANNEL_SIZE {
        attach.write(Message::Data(data.clone(), Pipe::StdOut));
        task::yield_now().await;
    }
    // Nothing tells that it waits, a loop which gives up later passes.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!write_loop.is_finished());

    // Neither the output nor the done packet wait for it after the exit.
    token.cancel();
    timeout(Duration::from_secs(5), write_loop).await???;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_loop_drains_after_exit_with_busy_output() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let _output = attach.output();
    let (server, client) = seqpacket_pair()?;
    let (rx, _) = attach.subscribe();
    let token = CancellationToken::new();

    // A process left behind which never pauses.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = std::thread::spawn({
        let (attach, stop) = (attach.clone(), stop.clone());
        move || {
            while !stop.load(Ordering::Relaxed) {
                attach.write(Message::Data(Bytes::from_static(b"x"), Pipe::StdOut));
            }
        }
    });
    let write_loop = task::spawn(Attach::write_loop(server, rx, false, token.clone()));
    tokio::time::sleep(Duration::from_millis(10)).await;
    token.cancel();

    let res = timeout(Duration::from_secs(10), async {
        while recv_packet(&client).await? != Attach::DONE_PACKET {}
        Ok::<_, anyhow::Error>(())
    })
    .await;
    stop.store(true, Ordering::Relaxed);
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("writer panicked"))?;
    res??;
    timeout(Duration::from_secs(5), write_loop).await???;
    Ok(())
}

#[tokio::test]
async fn send_stdin_after_close() -> Result<()> {
    let attach = SharedContainerAttach::default();
    ContainerIO::read_loop_stdin(tokio::io::sink(), attach.clone(), {
        let token = CancellationToken::new();
        token.cancel();
        token
    })
    .await?;

    // More packets than the channel capacity, which get dropped instead of
    // waiting for a reader forever.
    for _ in 0..64 {
        timeout(
            Duration::from_secs(5),
            attach.send_stdin(Bytes::from_static(b"data")),
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn read_loop_forwards_packets_with_null_bytes() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let token = CancellationToken::new();
    let (client, task) = spawn_read_loop(&attach, &token)?;

    for packet in [&b"a\0b"[..], b"\0", b"c"] {
        client.send(packet).await?;
    }

    for expected in [&b"a\0b"[..], b"\0", b"c"] {
        let data = timeout(Duration::from_secs(5), attach.read()).await??;
        assert_eq!(data, expected);
    }

    token.cancel();
    task.await??;
    Ok(())
}

#[tokio::test]
async fn read_loop_forwards_large_packets() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let token = CancellationToken::new();
    let (client, task) = spawn_read_loop(&attach, &token)?;

    let packets = [
        vec![1],
        vec![2; Attach::PACKET_BUF_SIZE],
        vec![3; 4 * Attach::PACKET_BUF_SIZE],
        vec![4; Attach::STDIN_PACKET_MAX_SIZE],
    ];
    for packet in &packets {
        client.send(packet).await?;
    }
    for expected in &packets {
        let data = timeout(Duration::from_secs(5), attach.read()).await??;
        assert_eq!(data, expected);
    }

    // Larger packets get truncated.
    client
        .send(&vec![5; Attach::STDIN_PACKET_MAX_SIZE + 1])
        .await?;
    let data = timeout(Duration::from_secs(5), attach.read()).await??;
    assert_eq!(data, vec![5; Attach::STDIN_PACKET_MAX_SIZE]);

    token.cancel();
    task.await??;
    Ok(())
}

#[tokio::test]
async fn read_loop_forwards_data_after_empty_packets() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let token = CancellationToken::new();
    let (client, task) = spawn_read_loop(&attach, &token)?;

    // All of it is queued before the read loop runs, so the client closed
    // its write side already when the empty packets get read.
    for packet in [&b""[..], b"z", b"", b"y"] {
        client.send(packet).await?;
    }
    client.shutdown(Shutdown::Write)?;

    for expected in [&b"z"[..], b"y"] {
        let data = timeout(Duration::from_secs(5), attach.read()).await??;
        assert_eq!(data, expected);
    }
    timeout(Duration::from_secs(5), task).await???;
    Ok(())
}

#[tokio::test]
async fn read_loop_continues_after_empty_packet() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let token = CancellationToken::new();
    let (client, task) = spawn_read_loop(&attach, &token)?;

    // The data after it still gets read.
    client.send(b"").await?;
    client.send(b"a").await?;
    let data = timeout(Duration::from_secs(5), attach.read()).await??;
    assert_eq!(data, &b"a"[..]);
    assert!(!task.is_finished());

    token.cancel();
    task.await??;
    Ok(())
}

#[tokio::test]
async fn stdin_eof_keeps_listener() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("attach");
    let token = CancellationToken::new();
    let attach = SharedContainerAttach::default();
    attach.add(&path, token.clone(), true)?;
    // Only the owner may connect.
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o077, 0);

    let connect_client = || -> Result<OwnedFd> {
        let fd = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )?;
        connect(fd.as_raw_fd(), &UnixAddr::new(&path)?)?;
        Ok(fd)
    };

    // A client which stops after stdin EOF closes the stdin of the
    // container, but the container keeps running. All of its input is
    // queued before the connection gets accepted.
    let first = connect_client()?;
    for packet in [&b""[..], b"x"] {
        send(first.as_raw_fd(), packet, MsgFlags::empty())?;
    }
    nix::sys::socket::shutdown(first.as_raw_fd(), nix::sys::socket::Shutdown::Write)?;
    let data = timeout(Duration::from_secs(5), attach.read()).await??;
    assert_eq!(data, &b"x"[..]);
    timeout(Duration::from_secs(5), attach.stdin_closed().cancelled()).await?;
    assert!(!token.is_cancelled());

    // Later clients still get attached.
    let _second = connect_client()?;

    // The exit stops the listener and removes the socket.
    token.cancel();
    wait_for(|| !path.exists()).await?;
    Ok(())
}

/// Connect to an attach socket and wait until the connection receives
/// the output.
async fn attach_client(attach: &SharedContainerAttach, path: &Path) -> Result<UnixSeqpacket> {
    let readers = attach.receiver_count();
    let client = UnixSeqpacket::connect(path).await?;
    wait_for(|| attach.receiver_count() > readers).await?;
    Ok(client)
}

#[tokio::test]
async fn listens_again_once_the_socket_got_removed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("attach");
    let token = CancellationToken::new();
    let attach = SharedContainerAttach::default();
    attach.add(&path, token.clone(), false)?;
    let ino = fs::metadata(&path)?.ino();
    let first_stop = lock(&attach.listeners)
        .get(&path)
        .context("no listener")?
        .stop
        .clone();

    // Another attach keeps the listener.
    attach.add(&path, token.clone(), false)?;
    assert_eq!(fs::metadata(&path)?.ino(), ino);
    assert!(!first_stop.is_cancelled());
    let first = attach_client(&attach, &path).await?;

    // Older clients removed the socket once they were done, nobody could
    // connect to the listener anymore.
    fs::remove_file(&path)?;
    attach.add(&path, token.clone(), false)?;
    assert!(first_stop.is_cancelled());
    assert_eq!(lock(&attach.listeners).len(), 1);
    let second = attach_client(&attach, &path).await?;

    // The connections of the stopped listener stay.
    attach.write(Message::Data(Bytes::from_static(b"x"), Pipe::StdOut));
    assert_eq!(recv_packet(&first).await?, b"\x02x");
    assert_eq!(recv_packet(&second).await?, b"\x02x");

    // The stopped listener leaves the new socket alone, the exit removes
    // it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(path.exists());
    token.cancel();
    wait_for(|| !path.exists()).await?;
    wait_for(|| lock(&attach.listeners).is_empty()).await?;
    Ok(())
}

#[tokio::test]
async fn ended_listener_removes_its_entry_and_socket() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("attach");
    let attach = SharedContainerAttach::default();
    attach.add(&path, CancellationToken::new(), false)?;
    let id = lock(&attach.listeners)
        .get(&path)
        .context("no listener")?
        .id;
    let cleanup = |id| ListenerCleanup {
        attach: attach.clone(),
        path: path.clone(),
        id,
    };

    // One which got replaced, so of another id, leaves them alone.
    drop(cleanup(id + 1));
    assert!(lock(&attach.listeners).contains_key(&path));
    assert!(path.exists());

    // Any other end of the task removes both, so that the next attach
    // listens again instead of taking the dead socket for a live one.
    drop(cleanup(id));
    assert!(lock(&attach.listeners).is_empty());
    assert!(!path.exists());
    attach.add(&path, CancellationToken::new(), false)?;
    attach_client(&attach, &path).await?;
    Ok(())
}

#[tokio::test]
async fn replaces_a_stale_socket() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("attach");
    let token = CancellationToken::new();

    // A socket left behind by a previous server.
    drop(std::os::unix::net::UnixListener::bind(&path)?);
    let attach = SharedContainerAttach::default();
    attach.add(&path, token.clone(), false)?;
    attach_client(&attach, &path).await?;
    token.cancel();
    wait_for(|| !path.exists()).await?;

    // Anything else stays.
    fs::write(&path, b"")?;
    let attach = SharedContainerAttach::default();
    assert!(attach.add(&path, CancellationToken::new(), false).is_err());
    assert!(path.exists());
    Ok(())
}

#[tokio::test]
async fn read_loop_stops_while_waiting_for_stdin() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let token = CancellationToken::new();
    let (client, read_loop) = spawn_read_loop(&attach, &token)?;

    // Nothing reads the stdin, so the read loop ends up waiting to send.
    for i in 0..=STDIN_CHANNEL_SIZE {
        client.send(&[u8::try_from(i)? + 1]).await?;
    }
    timeout(Duration::from_secs(5), async {
        while attach.read_half_tx.len() < STDIN_CHANNEL_SIZE {
            task::yield_now().await;
        }
    })
    .await?;

    token.cancel();
    timeout(Duration::from_secs(5), read_loop).await???;
    Ok(())
}

#[tokio::test]
async fn read_loop_stops_on_eof() -> Result<()> {
    for (terminal, stop_after_stdin_eof) in [(false, false), (false, true), (true, true)] {
        let attach = SharedContainerAttach::new(terminal);
        attach
            .stop_after_stdin_eof
            .store(stop_after_stdin_eof, Ordering::Release);

        let (server, client) = seqpacket_pair()?;
        drop(client);

        timeout(
            Duration::from_secs(5),
            Attach::read_loop(server, attach.clone(), CancellationToken::new()),
        )
        .await??;
        assert_eq!(
            attach.stdin_closed.is_cancelled(),
            stop_after_stdin_eof && !terminal
        );
    }
    Ok(())
}

#[tokio::test]
async fn read_loop_forwards_stdin_after_reset() -> Result<()> {
    let attach = SharedContainerAttach::default();
    attach.stop_after_stdin_eof.store(true, Ordering::Release);
    let (server, client) = seqpacket_pair()?;

    // Closing a socket with unread data resets the connection, which the
    // next read reports before the queued stdin.
    server.send(b"output").await?;
    client.send(b"x").await?;
    drop(client);

    let task = task::spawn(Attach::read_loop(
        server,
        attach.clone(),
        CancellationToken::new(),
    ));
    let data = timeout(Duration::from_secs(5), attach.read()).await??;
    assert_eq!(data, &b"x"[..]);
    timeout(Duration::from_secs(5), task).await???;
    assert!(attach.stdin_closed.is_cancelled());
    Ok(())
}

#[tokio::test]
async fn stdin_without_dropping() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let (client, read_loop) = spawn_read_loop(&attach, &CancellationToken::new())?;

    // More packets than the channel capacity, which have to wait for the
    // reader instead of getting dropped.
    const PACKETS: u8 = 64;
    for i in 0..PACKETS {
        client.send(&[i + 1]).await?;
    }
    drop(client);

    let mut received = vec![];
    while let Ok(data) = timeout(Duration::from_secs(5), attach.read()).await? {
        received.extend_from_slice(&data);
        if received.len() == usize::from(PACKETS) {
            break;
        }
    }
    assert_eq!(received, (1..=PACKETS).collect::<Vec<_>>());

    read_loop.await??;
    Ok(())
}
