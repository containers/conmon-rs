use super::*;
use crate::{
    container_log::{
        LineBuffer,
        testing::{Journal, receive},
    },
    journal::testing::decode,
};
use anyhow::bail;
use std::os::unix::net::UnixDatagram;

type Fields = Vec<(String, Vec<u8>)>;

fn field(key: &str, value: &str) -> (String, Vec<u8>) {
    (key.to_owned(), value.as_bytes().to_vec())
}

/// The entries the logger sent to the journal. Writing waits for the
/// sends, so they are all queued already.
fn received(socket: &UnixDatagram) -> Result<Vec<Fields>> {
    Ok(receive(socket)?.iter().map(|entry| decode(entry)).collect())
}

/// A journal whose queue is full, so that it takes no entry right away.
fn full_journal() -> Result<Journal> {
    let journal = Journal::new()?;
    let error = journal::tests::fill(Wait::Never, &journal.path);
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    Ok(journal)
}

/// Wait in a thread of its own until the journal got an entry with the
/// `message`, after it caught up a little later. The thread keeps the
/// journal, its directory with the socket stays until then.
fn catch_up(journal: Journal, message: &'static str) -> std::thread::JoinHandle<Result<bool>> {
    std::thread::spawn(move || -> Result<bool> {
        // The whole journal, not only its socket.
        let journal = &journal;
        std::thread::sleep(std::time::Duration::from_millis(100));
        let socket = &journal.socket;
        socket.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let mut buf = vec![0; 4096];
        loop {
            let len = socket.recv(&mut buf)?;
            if decode(&buf[..len]).contains(&field("MESSAGE", message)) {
                return Ok(true);
            }
        }
    })
}

#[test]
fn priorities() {
    assert_eq!(JournaldLogger::priority(Pipe::StdOut), Priority::Info);
    assert_eq!(JournaldLogger::priority(Pipe::StdErr), Priority::Error);
}

#[test]
fn short_ids_are_kept() -> Result<()> {
    let logger = JournaldLogger::new("abc", "");
    assert_eq!(
        decode(logger.shared.fields.as_bytes()),
        [
            field("CONTAINER_ID_FULL", "abc"),
            field("CONTAINER_ID", "abc"),
            field("SYSLOG_IDENTIFIER", "abc"),
        ]
    );
    Ok(())
}

#[test]
fn syslog_identifier_like_conmon() {
    let id = "0123456789abcdef";
    let identifier = |name| {
        let logger = JournaldLogger::new(id, name);
        decode(logger.shared.fields.as_bytes()).last().cloned()
    };
    // The name, as CRI-O passes it, then the short ID.
    assert_eq!(
        identifier("k8s_ctr"),
        Some(field("SYSLOG_IDENTIFIER", "k8s_ctr"))
    );
    assert_eq!(
        identifier(""),
        Some(field("SYSLOG_IDENTIFIER", "0123456789ab"))
    );
}

#[tokio::test]
async fn name_comes_from_the_request() -> Result<()> {
    use crate::{
        conmon_capnp::conmon::{create_container_request, log_driver::Type},
        container_log::{ContainerLog, Driver},
    };

    let mut message = capnp::message::Builder::new_default();
    let mut drivers = message
        .init_root::<create_container_request::Builder<'_>>()
        .init_log_drivers(1);
    let mut driver = drivers.reborrow().get(0);
    driver.set_type(Type::Journald);
    driver.set_container_name("k8s_ctr");

    let log = ContainerLog::from_request("abc", drivers.into_reader())?;
    let log = log.lock().await;
    let [Driver::Journald(logger)] = log.drivers.as_slice() else {
        bail!("no journald driver");
    };
    assert_eq!(
        decode(logger.shared.fields.as_bytes()),
        [
            field("CONTAINER_ID_FULL", "abc"),
            field("CONTAINER_ID", "abc"),
            field("CONTAINER_NAME", "k8s_ctr"),
            field("SYSLOG_IDENTIFIER", "k8s_ctr"),
        ]
    );
    Ok(())
}

/// The entries carry the message, the priority of the pipe, the
/// container fields and the partial marker of unfinished lines.
#[tokio::test]
async fn write_lines_sends_entries() -> Result<()> {
    let journal = Journal::new()?;
    let logger = JournaldLogger::with_socket("0123456789abcdef", &journal.path);

    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"Test log message\nand another");
    logger.write_lines(Pipe::StdOut, &lines).await?;
    let lines = buffer.finish(Pipe::StdOut);
    assert!(lines[0].partial);
    logger.write_lines(Pipe::StdErr, &lines).await?;

    let entry = |priority: &str, message: &str, partial: bool| {
        let mut fields = vec![
            field("PRIORITY", priority),
            field("MESSAGE", message),
            field("CONTAINER_ID_FULL", "0123456789abcdef"),
            field("CONTAINER_ID", "0123456789ab"),
            field("SYSLOG_IDENTIFIER", "0123456789ab"),
        ];
        if partial {
            fields.push(field("CONTAINER_PARTIAL_MESSAGE", "true"));
        }
        fields
    };
    assert_eq!(
        received(&journal.socket)?,
        [
            entry("6", "Test log message", false),
            entry("3", "and another", true),
        ]
    );
    Ok(())
}

/// The output goes to the journal as it is, like with conmon.
#[tokio::test]
async fn binary_output_is_kept() -> Result<()> {
    let journal = Journal::new()?;
    let logger = JournaldLogger::with_socket("test", &journal.path);

    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"\xff\xfe\x00bin\n");
    logger.write_lines(Pipe::StdOut, &lines).await?;

    let entries = received(&journal.socket)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0][1],
        ("MESSAGE".into(), b"\xff\xfe\x00bin".to_vec())
    );
    Ok(())
}

/// The lines which a busy journal does not take right away wait for it.
#[tokio::test]
async fn lines_wait_for_a_busy_journal() -> Result<()> {
    let journal = full_journal()?;

    // The journal catches up a little later.
    let path = journal.path.clone();
    let reader = catch_up(journal, "late");
    let logger = JournaldLogger::with_socket("test", &path);
    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"late\n");
    logger.write_lines(Pipe::StdOut, &lines).await?;
    assert!(
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("reader panicked"))??
    );
    Ok(())
}

/// Once the lines of a chunk did not go out in time, the following chunks
/// do not wait for a journal which takes nothing.
#[tokio::test]
async fn chunks_after_a_timeout_do_not_wait() -> Result<()> {
    let journal = full_journal()?;

    let logger = JournaldLogger::with_socket("test", &journal.path);
    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"waits\n");
    logger.write_lines(Pipe::StdOut, &lines).await?;
    assert!(lock(&logger.shared.timed_out).is_some());

    // Also after the minimum time without waiting.
    *lock(&logger.shared.timed_out) = Instant::now().checked_sub(CHUNK_TIMEOUT);
    let lines = buffer.push(Pipe::StdOut, b"dropped\n");
    let start = Instant::now();
    logger.write_lines(Pipe::StdOut, &lines).await?;
    // Waiting would take a `journal::SEND_TIMEOUT`, as long as the chunk.
    assert!(start.elapsed() < CHUNK_TIMEOUT);
    assert!(lock(&logger.shared.timed_out).is_some());

    // The journal catches up.
    let entries = received(&journal.socket)?;
    assert!(
        !entries
            .iter()
            .any(|entry| entry.contains(&field("MESSAGE", "dropped")))
    );
    let lines = buffer.push(Pipe::StdOut, b"taken\n");
    logger.write_lines(Pipe::StdOut, &lines).await?;
    assert_eq!(received(&journal.socket)?.len(), 1);
    Ok(())
}

/// A journal which takes part of a chunk shortly after a timeout does not
/// keep up, so the rest of the chunk does not wait for it either.
#[tokio::test]
async fn partly_taken_chunks_after_a_timeout_do_not_wait() -> Result<()> {
    let journal = full_journal()?;
    // Room for a single entry.
    journal.socket.recv(&mut [0; 4096])?;

    let logger = JournaldLogger::with_socket("test", &journal.path);
    *lock(&logger.shared.timed_out) = Some(Instant::now());
    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"taken\ndropped\n");
    let start = Instant::now();
    logger.write_lines(Pipe::StdOut, &lines).await?;
    assert!(start.elapsed() < CHUNK_TIMEOUT);
    assert!(lock(&logger.shared.timed_out).is_some());
    Ok(())
}

/// A journal which still takes lines a while after a timeout is slow, not
/// stalled, so the rest of a chunk waits for it again.
#[tokio::test]
async fn partly_taken_chunks_wait_again() -> Result<()> {
    let journal = full_journal()?;
    // Room for a single entry.
    journal.socket.recv(&mut [0; 4096])?;

    // The journal catches up a little later.
    let path = journal.path.clone();
    let reader = catch_up(journal, "waits");
    let logger = JournaldLogger::with_socket("test", &path);
    *lock(&logger.shared.timed_out) = Instant::now().checked_sub(CHUNK_TIMEOUT);
    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"taken\nwaits\n");
    logger.write_lines(Pipe::StdOut, &lines).await?;
    assert!(lock(&logger.shared.timed_out).is_none());
    assert!(
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("reader panicked"))??
    );
    Ok(())
}

/// The lines which did not go out by the deadline get dropped.
#[test]
fn lines_after_the_deadline_get_dropped() -> Result<()> {
    let journal = Journal::new()?;
    let logger = JournaldLogger::with_socket("test", &journal.path);
    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"one\ntwo\n");
    let past = Instant::now();
    let handled = JournaldLogger::send(&logger.shared, Pipe::StdOut, &lines, Wait::Bounded(past));
    assert_eq!(handled, 2);
    assert!(logger.shared.failing.load(Ordering::Relaxed));
    assert!(lock(&logger.shared.timed_out).is_some());
    assert!(received(&journal.socket)?.is_empty());
    Ok(())
}

/// Failing sends get logged once and do not fail the write.
#[tokio::test]
async fn send_failures_are_not_fatal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let logger = JournaldLogger::with_socket("test", &path);
    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"lost\n");
    logger.write_lines(Pipe::StdOut, &lines).await?;
    assert!(logger.shared.failing.load(Ordering::Relaxed));
    logger.write_lines(Pipe::StdOut, &lines).await?;
    assert!(logger.shared.failing.load(Ordering::Relaxed));

    // The journal is back.
    let socket = UnixDatagram::bind(&path)?;
    logger.write_lines(Pipe::StdOut, &lines).await?;
    assert!(!logger.shared.failing.load(Ordering::Relaxed));
    assert_eq!(received(&socket)?.len(), 1);
    Ok(())
}

/// A journal which takes some lines of a chunk is not back yet.
#[test]
fn partly_sent_chunks_keep_failing() -> Result<()> {
    let journal = full_journal()?;
    // Room for a single entry.
    journal.socket.recv(&mut [0; 4096])?;

    let logger = JournaldLogger::with_socket("test", &journal.path);
    logger.shared.failing.store(true, Ordering::Relaxed);
    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"one\ntwo\n");
    let handled = JournaldLogger::send(&logger.shared, Pipe::StdOut, &lines, Wait::Never);
    assert_eq!(handled, 1);
    assert!(logger.shared.failing.load(Ordering::Relaxed));
    Ok(())
}
