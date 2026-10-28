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
    (0..251).cycle().take(len).collect()
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

#[tokio::test(start_paused = true)]
async fn runtime_errors_stop_the_output() -> Result<()> {
    // A runtime which left a process behind holding the pipes.
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
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
