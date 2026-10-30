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

/// Distinct output, so that a lost or reordered chunk is noticed. Built
/// from whole blocks, byte by byte takes long in emulated tests.
fn pattern(len: usize) -> Vec<u8> {
    let block: Vec<u8> = (0..251).collect();
    let mut pattern = block.repeat(len.div_ceil(block.len()));
    pattern.truncate(len);
    pattern
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
    let stdio = ContainerIO::read_stream(&CancellationToken::new(), &rx, early).await;
    assert_eq!(stdio, b"early");
    assert!(rx.is_closed());
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_forward_the_output() -> Result<()> {
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let token = CancellationToken::new();
    let expected = "hello world";
    let mut child = tokio::process::Command::new("echo")
        .arg("-n")
        .arg(expected)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    io.connect_streams(
        child.stdin.take(),
        child.stdout.take(),
        child.stderr.take(),
        token.clone(),
    );
    let (stdout_rx, stderr_rx) = io.stdio()?;

    let msg = stdout_rx.recv().await.context("no message on stdout")?;
    assert_eq!(msg, Message::Data(expected.into(), Pipe::StdOut));

    // The token only stops the stdin loop, the output ends with the
    // process, which exits after its echo.
    token.cancel();

    let msg = stdout_rx.recv().await.context("no message on stdout")?;
    assert_eq!(msg, Message::Done);
    assert!(stdout_rx.try_recv().is_err());

    let msg = stderr_rx.recv().await.context("no message on stderr")?;
    assert_eq!(msg, Message::Done);
    assert!(stderr_rx.try_recv().is_err());
    Ok(())
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
    io.connect_streams(
        None,
        child.stdout.take(),
        child.stderr.take(),
        token.clone(),
    );
    child.wait().await?;

    // Not cancelled, so that the rest gets read until the output ended
    // instead of as long as a drain waits for it.
    let (stdout, _) = io.read_all(&token).await?;
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
    let stdio = ContainerIO::read_stream(&exited, &rx, EarlyStream::default()).await;
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

#[tokio::test(start_paused = true)]
async fn read_loop_flushes_trickling_line() -> Result<()> {
    let (_cri, json, logger) = test_helpers::container_log(true).await?;
    let (mut writer, _rx, read_loop) = spawn_loop(logger, false);

    // Progress output which never pauses as long as the line waits.
    let mut flushed = false;
    for _ in 0..10 {
        writer.write_all(b".").await?;
        time::sleep(ContainerIO::IDLE_FLUSH * 4 / 5).await;
        if test_helpers::read(&json)?.contains(r#""partial":true"#) {
            flushed = true;
            break;
        }
    }
    assert!(flushed, "trickling line not flushed");

    drop(writer);
    read_loop.await?
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
    assert!(logger.try_lock().is_ok());
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
async fn stopped_output_ends_read_while_pipe_is_held_open() -> Result<()> {
    // The writer stays, like a process left behind holding the pipe.
    let (mut writer, reader) = duplex(1024);
    let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
    let sender = StdioSender::new(tx, &Arc::new(AtomicBool::new(true)));
    let attach = SharedContainerAttach::default();
    let read_loop = task::spawn(ContainerIO::read_loop(
        reader,
        Pipe::StdOut,
        ContainerLog::new(),
        Some(sender),
        attach.clone(),
        attach.output(),
    ));
    writer.write_all(b"data").await?;
    assert_eq!(data(rx.recv().await?), "data");

    attach.stop_output();
    assert_eq!(time::timeout(WAIT, rx.recv()).await??, Message::Done);
    time::timeout(WAIT, read_loop).await???;
    assert!(attach.subscribe().1, "output did not end");
    // Its writes fail once nothing reads the pipe anymore.
    assert!(writer.write_all(b"more").await.is_err());
    Ok(())
}

#[tokio::test]
async fn dropped_stdio_frees_the_channels_of_quiet_pipes() -> Result<()> {
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let (mut stdout, stdout_reader) = duplex(1024);
    let (_stderr, stderr_reader) = duplex(1024);
    let output = io.attach.output();
    io.spawn_read_loop(stdout_reader, Pipe::StdOut, output.clone());
    io.spawn_read_loop(stderr_reader, Pipe::StdErr, output);
    let (stdout_rx, stderr_rx) = io.stdio()?;
    stdout.write_all(b"created").await?;
    assert_eq!(data(stdout_rx.recv().await?), "created");

    // The pipes stay open, but nothing gets written to them anymore.
    io.drop_stdio();
    time::timeout(WAIT, async {
        while stdout_rx.sender_count() + stderr_rx.sender_count() > 0 {
            task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn runtime_errors_stop_the_output() -> Result<()> {
    // A runtime which left a process behind holding the pipes.
    let io = ContainerIO::new(false, ContainerLog::new())?;
    let e = io.runtime_error(anyhow::anyhow!("failed")).await;
    assert_eq!(format!("{e:#}"), "failed");
    assert!(io.attach().output_stopped().is_cancelled());
    Ok(())
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

    let stdout =
        ContainerIO::read_stream(&CancellationToken::new(), &rx, EarlyStream::default()).await;
    assert!(stdout.len() < ContainerIO::MAX_STDIO_STREAM_SIZE);
    // At most the chunk which crossed the limit is missing.
    assert!(stdout.len() > ContainerIO::MAX_STDIO_STREAM_SIZE - 64 * 1024);
    assert!(stdout == pattern(stdout.len()));

    // The rest gets discarded instead of waiting for the consumer.
    time::timeout(WAIT, write).await???;
    time::timeout(WAIT, read_loop).await??
}

#[tokio::test(start_paused = true)]
async fn read_stream_keeps_reading_until_the_exit() {
    // The watcher of the process tells when it exited, or killed it at its
    // timeout, the output written right before that belongs to it.
    let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
    let exited = CancellationToken::new();
    let writer = tokio::spawn({
        let exited = exited.clone();
        async move {
            time::sleep(Duration::from_secs(3600)).await;
            tx.send(Message::Data(Bytes::from_static(b"last"), Pipe::StdOut))
                .await
                .expect("send");
            exited.cancel();
            // A process left behind keeps the pipe open.
            time::sleep(Duration::from_secs(3600)).await;
            drop(tx);
        }
    });

    let start = Instant::now();
    let stdio = ContainerIO::read_stream(&exited, &rx, EarlyStream::default()).await;
    assert_eq!(stdio, b"last");
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_secs(3600), "{elapsed:?}");
    assert!(
        elapsed <= Duration::from_secs(3600) + DRAIN_MAX_WAIT,
        "{elapsed:?}"
    );
    writer.abort();
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
        async move { ContainerIO::read_stream(&exited, &rx, EarlyStream::default()).await }
    });
    task::yield_now().await;
    exited.cancel();
    // Output which was still in flight at the exit is taken.
    tx.send(Message::Data("exit".into(), Pipe::StdOut)).await?;

    let start = Instant::now();
    let stdout = read.await?;
    assert_eq!(stdout, b"before exit");
    assert!(start.elapsed() < Duration::from_secs(1));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn closed_channel_ends_the_output() -> Result<()> {
    let (tx, rx) = async_channel::bounded(STDIO_CHANNEL_SIZE);
    tx.send(Message::Data("data".into(), Pipe::StdOut)).await?;
    drop(tx);

    let start = Instant::now();
    let stdout =
        ContainerIO::read_stream(&CancellationToken::new(), &rx, EarlyStream::default()).await;
    assert_eq!(stdout, b"data");
    assert_eq!(start.elapsed(), Duration::ZERO);
    Ok(())
}

#[tokio::test]
async fn closed_stdin_ends_the_stdin_loop() -> Result<()> {
    // The process exited or closed its stdin.
    let (stdin, process) = tokio::net::UnixStream::pair()?;
    drop(process);
    let attach = SharedContainerAttach::default();
    let stdin_loop = task::spawn(ContainerIO::read_loop_stdin(
        stdin,
        attach.clone(),
        CancellationToken::new(),
    ));
    attach.send_stdin(Bytes::from_static(b"late")).await;
    time::timeout(WAIT, stdin_loop).await???;

    // The input which follows gets dropped.
    time::timeout(WAIT, attach.send_stdin(Bytes::from_static(b"dropped"))).await?;
    Ok(())
}

#[test]
fn lag_lasts_until_the_client_caught_up() -> Result<()> {
    let (tx, mut rx) = broadcast::channel(2);
    let mut lag = Lag::default();
    lag.skipped(3);
    lag.skipped(4);
    assert_eq!(lag.skipped, Some(7));

    tx.send(1)?;
    tx.send(2)?;
    assert_eq!(rx.try_recv()?, 1);
    lag.received(&rx);
    assert_eq!(lag.skipped, Some(7));
    assert_eq!(rx.try_recv()?, 2);
    lag.received(&rx);
    assert_eq!(lag.skipped, None);
    Ok(())
}

#[tokio::test]
async fn drain_keeps_the_lag_of_the_client() -> Result<()> {
    // A client which fell behind before the exit skips more while draining.
    let (tx, mut rx) = broadcast::channel(1);
    let mut lag = Lag::default();
    lag.skipped(3);
    let data = || Message::Data(Bytes::from_static(b"data"), Pipe::StdOut);
    tx.send(data())?;
    tx.send(data())?;
    tx.send(Message::Done)?;

    let mut drain = Drain::default();
    assert!(drain.recv_output(&mut rx, Some(&mut lag)).await.is_none());
    assert_eq!(lag.skipped, Some(5));
    Ok(())
}
