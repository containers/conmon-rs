//! Waiting for the output and the log once the process exited.

use super::*;

#[tokio::test(start_paused = true)]
async fn exit_files_wait_for_quiet_output() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let output = attach.output();

    // Nothing flows, a process holds the pipes open.
    let start = Instant::now();
    assert!(
        ReapableChild::wait_for_output(attach.clone(), Instant::now() + DRAIN_MAX_LOG_WAIT).await
    );
    assert_eq!(start.elapsed(), DRAIN_TIMEOUT);

    // Output which keeps flowing is bounded.
    let writer = task::spawn({
        let attach = attach.clone();
        async move {
            loop {
                attach.write(Message::Data(
                    "data".into(),
                    crate::container_io::Pipe::StdOut,
                ));
                time::sleep(DRAIN_TIMEOUT / 2).await;
            }
        }
    });
    let start = Instant::now();
    assert!(
        ReapableChild::wait_for_output(attach.clone(), Instant::now() + DRAIN_MAX_LOG_WAIT).await
    );
    assert_eq!(start.elapsed(), DRAIN_MAX_WAIT);
    writer.abort();

    // A deadline which passes while the drain waits ends the wait.
    let start = Instant::now();
    assert!(!ReapableChild::wait_for_output(attach.clone(), start + DRAIN_TIMEOUT / 2).await);
    assert_eq!(start.elapsed(), DRAIN_TIMEOUT / 2);

    // Ended output does not wait at all.
    drop(output);
    let start = Instant::now();
    assert!(ReapableChild::wait_for_output(attach, Instant::now() + DRAIN_MAX_LOG_WAIT).await);
    assert_eq!(start.elapsed(), Duration::ZERO);
    Ok(())
}

/// A read loop over a pipe, whose log drivers hang while a guard from
/// [`hold`] holds the log.
struct LogLoop {
    writer: tokio::io::DuplexStream,
    attach: SharedContainerAttach,
    logger: crate::container_log::SharedContainerLog,
    cri: tempfile::NamedTempFile,
    lines: crate::container_log::test_helpers::Journal,
    task: JoinHandle<Result<()>>,
}

async fn log_loop() -> Result<LogLoop> {
    let (cri, lines, logger) = crate::container_log::test_helpers::container_log(true).await?;
    let attach = SharedContainerAttach::default();
    let (writer, reader) = tokio::io::duplex(1024);
    let task = task::spawn(ContainerIO::read_loop(
        reader,
        crate::container_io::Pipe::StdOut,
        logger.clone(),
        None,
        attach.clone(),
        attach.output(),
    ));
    // The loop looks up the drivers first, which a held log would block.
    task::yield_now().await;
    Ok(LogLoop {
        writer,
        attach,
        logger,
        cri,
        lines,
        task,
    })
}

/// Make the log drivers hang until the guard gets dropped.
async fn hold(
    logger: &crate::container_log::SharedContainerLog,
) -> tokio::sync::OwnedMutexGuard<ContainerLog> {
    logger.clone().lock_owned().await
}

#[tokio::test(start_paused = true)]
async fn exit_files_wait_for_slow_log_drivers() -> Result<()> {
    let mut l = log_loop().await?;
    let held = hold(&l.logger).await;
    l.writer.write_all(b"last line\n").await?;
    wait_for(|| l.attach.is_logging()).await?;

    // The log takes longer than the drain waits for a pause.
    let wait = task::spawn(ReapableChild::wait_for_output(
        l.attach.clone(),
        Instant::now() + DRAIN_MAX_LOG_WAIT,
    ));
    time::sleep(2 * DRAIN_MAX_WAIT).await;
    assert!(!wait.is_finished());

    drop(held);
    drop(l.writer);
    assert!(wait.await?);
    assert!(crate::container_log::test_helpers::read(&l.cri)?.contains("last line"));
    l.task.await?
}

#[tokio::test(start_paused = true)]
async fn exit_files_wait_for_the_last_line() -> Result<()> {
    let mut l = log_loop().await?;
    let (mut rx, _) = l.attach.subscribe();
    l.writer.write_all(b"no newline").await?;
    // Broadcast once the chunk got logged, the line still waits.
    rx.recv().await?;

    // The output ended, its last line is on its way into the log.
    let held = hold(&l.logger).await;
    drop(l.writer);
    wait_for(|| l.attach.is_logging()).await?;
    let wait = task::spawn(ReapableChild::wait_for_output(
        l.attach.clone(),
        Instant::now() + DRAIN_MAX_LOG_WAIT,
    ));
    time::sleep(2 * DRAIN_MAX_WAIT).await;
    assert!(!wait.is_finished());

    drop(held);
    wait.await?;
    assert!(
        l.lines
            .read()?
            .contains(r#""message":"no newline","partial":true"#)
    );
    l.task.await?
}

#[tokio::test(start_paused = true)]
async fn nothing_gets_logged_after_the_output_finished() -> Result<()> {
    let mut l = log_loop().await?;
    let (mut rx, _) = l.attach.subscribe();
    l.writer.write_all(b"no newline").await?;
    rx.recv().await?;

    // A process left behind keeps the pipe open, the line waits for its
    // ending. It gets written once the read loop ended.
    ReapableChild::finish_output(l.attach.clone(), &l.logger).await;
    (&mut l.task).await??;
    let read = || l.lines.read();
    assert!(read()?.contains(r#""message":"no newline","partial":true"#));

    // Its later output does not reach the log anymore.
    assert!(l.writer.write_all(b"late\n").await.is_err());
    assert!(!read()?.contains("late"));
    Ok(())
}

/// The output of an exec whose consumer is slower than the drain after
/// the exit waits for a pause, which blocks the read loop, arrives in
/// full.
#[tokio::test(start_paused = true)]
async fn slow_exec_consumers_get_all_output() -> Result<()> {
    use crate::container_io::{STDIO_CHANNEL_SIZE, StdioSender};

    const OUTPUT: usize = 1024 * 1024;
    let reaper = ChildReaper::default();
    let dir = tempfile::tempdir()?;
    let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
    let attach = io.attach().clone();
    let (tx, rx) = crate::channel::bounded(STDIO_CHANNEL_SIZE);
    let sender = StdioSender::new(tx, &Arc::new(AtomicBool::new(true)));
    let (mut writer, reader) = tokio::io::duplex(64 * 1024);
    task::spawn(ContainerIO::read_loop(
        reader,
        crate::container_io::Pipe::StdOut,
        ContainerLog::new(),
        Some(sender),
        attach.clone(),
        attach.output(),
    ));
    let write = task::spawn(async move {
        writer.write_all(&vec![b'x'; OUTPUT]).await?;
        Ok::<_, std::io::Error>(writer)
    });

    // The process wrote its output and exited.
    let sh = spawn_sh("exit 0")?;
    let call = reaper.begin_call(distant_deadline())?;
    let mut child = child(sh.pid, None, &dir.path().join("exit"))?;
    child.kind = ChildKind::Exec;
    child.exit_paths = vec![];
    child.io = io.clone();
    let token = child.token.clone();
    let _stop = attach.stop_output_on_exit(token.clone());
    let mut exit_rx = reaper.watch_grandchild(child, call)?;
    exit_rx.recv().await?;

    let mut received = 0;
    while received < OUTPUT {
        time::sleep(4 * DRAIN_TIMEOUT).await;
        match rx.recv().await? {
            Message::Data(data, _) => received += data.len(),
            Message::Done => break,
        }
    }
    assert_eq!(received, OUTPUT);
    drop(write.await??);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn finished_output_is_bounded_with_a_hanging_log() -> Result<()> {
    let mut l = log_loop().await?;
    let held = hold(&l.logger).await;
    l.writer.write_all(b"never logged\n").await?;
    wait_for(|| l.attach.is_logging()).await?;

    let start = Instant::now();
    ReapableChild::finish_output(l.attach.clone(), &l.logger).await;
    assert_eq!(start.elapsed(), DRAIN_MAX_LOG_WAIT);

    drop(held);
    drop(l.writer);
    l.task.await?
}

#[tokio::test(start_paused = true)]
async fn finished_output_syncs_the_log_bounded() -> Result<()> {
    let mut l = log_loop().await?;
    l.writer.write_all(b"last\n").await?;
    drop(l.writer);
    l.task.await??;

    // The read loop ended, the log syncs without waiting for the output.
    let start = Instant::now();
    ReapableChild::finish_output(l.attach.clone(), &l.logger).await;
    assert_eq!(start.elapsed(), Duration::ZERO);

    // A log which does not get to sync does not hold the exit back either.
    let held = hold(&l.logger).await;
    ReapableChild::finish_output(l.attach, &l.logger).await;
    assert_eq!(start.elapsed(), DRAIN_MAX_LOG_WAIT);
    drop(held);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn exit_files_wait_for_log_drivers_bounded() -> Result<()> {
    let mut l = log_loop().await?;
    let held = hold(&l.logger).await;
    l.writer.write_all(b"never logged\n").await?;
    wait_for(|| l.attach.is_logging()).await?;

    let start = Instant::now();
    assert!(!ReapableChild::wait_for_output(l.attach, Instant::now() + DRAIN_MAX_LOG_WAIT).await);
    assert_eq!(start.elapsed(), DRAIN_MAX_LOG_WAIT);

    drop(held);
    drop(l.writer);
    l.task.await?
}
