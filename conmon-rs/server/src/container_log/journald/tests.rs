use super::*;
use crate::{container_log::LineBuffer, journal::testing::decode};
use std::os::unix::net::UnixDatagram;

type Fields = Vec<(String, Vec<u8>)>;

fn field(key: &str, value: &str) -> (String, Vec<u8>) {
    (key.to_owned(), value.as_bytes().to_vec())
}

impl JournaldLogger {
    fn with_socket(id: &str, socket: &Path) -> Self {
        let mut logger = Self::new(id, "", "", []).expect("logger");
        Arc::get_mut(&mut logger.shared)
            .expect("unshared logger")
            .socket = Some(socket.to_owned());
        logger
    }
}

/// The entries the logger sent to the socket. Writing waits for the
/// sends, so they are all queued already.
fn received(socket: &UnixDatagram) -> Result<Vec<Fields>> {
    socket.set_nonblocking(true)?;
    let mut entries = vec![];
    let mut buf = vec![0; 4096];
    loop {
        match socket.recv(&mut buf) {
            Ok(len) => entries.push(decode(&buf[..len])),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(entries),
            Err(e) => return Err(e.into()),
        }
    }
}

#[test]
fn priorities() {
    assert_eq!(JournaldLogger::priority(Pipe::StdOut), Priority::Info);
    assert_eq!(JournaldLogger::priority(Pipe::StdErr), Priority::Error);
}

#[test]
fn short_ids_are_kept() -> Result<()> {
    let logger = JournaldLogger::new("abc", "", "", [])?;
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
fn syslog_identifier_like_conmon() -> Result<()> {
    let id = "0123456789abcdef";
    let identifier = |name, tag| -> Result<_> {
        let logger = JournaldLogger::new(id, name, tag, [])?;
        let fields = decode(logger.shared.fields.as_bytes());
        Ok(fields.last().cloned())
    };
    // The tag, then the name, as CRI-O passes it, then the short ID.
    assert_eq!(
        identifier("k8s_ctr", "tag")?,
        Some(field("SYSLOG_IDENTIFIER", "tag"))
    );
    assert_eq!(
        identifier("k8s_ctr", "")?,
        Some(field("SYSLOG_IDENTIFIER", "k8s_ctr"))
    );
    assert_eq!(
        identifier("", "")?,
        Some(field("SYSLOG_IDENTIFIER", "0123456789ab"))
    );
    Ok(())
}

#[tokio::test]
async fn name_tag_and_labels_come_from_the_request() -> Result<()> {
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
    driver.set_tag("tag");
    let mut label = driver.init_labels(1).get(0);
    label.set_key("POD");
    label.set_value("web");

    let log = ContainerLog::from_request("abc", drivers.into_reader())?;
    let log = log.read().await;
    let [Driver::Journald(logger)] = log.drivers.as_slice() else {
        bail!("no journald driver");
    };
    assert_eq!(
        decode(logger.shared.fields.as_bytes()),
        [
            field("CONTAINER_ID_FULL", "abc"),
            field("CONTAINER_ID", "abc"),
            field("CONTAINER_NAME", "k8s_ctr"),
            field("CONTAINER_TAG", "tag"),
            field("SYSLOG_IDENTIFIER", "tag"),
            field("POD", "web"),
        ]
    );
    Ok(())
}

#[test]
fn name_tag_and_labels_like_conmon() -> Result<()> {
    let labels = [("POD", "web"), ("A_1", "")];
    let logger = JournaldLogger::new("abc", "k8s_ctr", "tag", labels)?;
    assert_eq!(
        decode(logger.shared.fields.as_bytes()),
        [
            field("CONTAINER_ID_FULL", "abc"),
            field("CONTAINER_ID", "abc"),
            field("CONTAINER_NAME", "k8s_ctr"),
            field("CONTAINER_TAG", "tag"),
            field("SYSLOG_IDENTIFIER", "tag"),
            field("POD", "web"),
            field("A_1", ""),
        ]
    );

    // As many labels as journald takes, with names and values within
    // the limit, but not more.
    let names: Vec<String> = (0..=JournaldLogger::MAX_LABELS)
        .map(|i| format!("L{i}"))
        .collect();
    let labels = |n: usize| names[..n].iter().map(|name| (name.as_str(), "v"));
    assert!(JournaldLogger::new("abc", "", "", labels(JournaldLogger::MAX_LABELS)).is_ok());
    let err = JournaldLogger::new("abc", "", "", labels(JournaldLogger::MAX_LABELS + 1))
        .err()
        .context("too many labels")?;
    assert!(
        err.to_string().contains("too many journald labels"),
        "{err}"
    );
    let large = "v".repeat(JournaldLogger::MAX_LABEL_BYTES);
    assert!(JournaldLogger::new("abc", "", "", [("LARGE", large.as_str())]).is_err());

    let long = "A".repeat(65);
    let invalid = ["", "lower", "_TRUSTED", "1A", "A-B", &long, "MESSAGE"];
    for key in invalid {
        assert!(
            JournaldLogger::new("abc", "", "", [(key, "x")]).is_err(),
            "{key}"
        );
    }
    Ok(())
}

/// The entries carry the message, the priority of the pipe, the
/// container fields and the partial marker of unfinished lines.
#[tokio::test]
async fn write_lines_sends_entries() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let socket = UnixDatagram::bind(&path)?;
    let mut logger = JournaldLogger::with_socket("0123456789abcdef", &path);
    logger.init().await?;

    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"Test log message\nand another");
    logger.write_lines(Pipe::StdOut, &lines).await?;
    logger.reopen().await?;
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
        received(&socket)?,
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
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let socket = UnixDatagram::bind(&path)?;
    let mut logger = JournaldLogger::with_socket("test", &path);

    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"\xff\xfe\x00bin\n");
    logger.write_lines(Pipe::StdOut, &lines).await?;

    let entries = received(&socket)?;
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
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let socket = UnixDatagram::bind(&path)?;
    let error = journal::tests::fill(Wait::Never, &path);
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);

    // The journal catches up a little later.
    let reader = std::thread::spawn(move || -> Result<bool> {
        std::thread::sleep(std::time::Duration::from_millis(100));
        socket.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let mut buf = vec![0; 4096];
        loop {
            let len = socket.recv(&mut buf)?;
            if decode(&buf[..len]).contains(&field("MESSAGE", "late")) {
                return Ok(true);
            }
        }
    });
    let mut logger = JournaldLogger::with_socket("test", &path);
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

/// The lines which did not go out by the deadline get dropped.
#[test]
fn lines_after_the_deadline_get_dropped() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let socket = UnixDatagram::bind(&path)?;
    let logger = JournaldLogger::with_socket("test", &path);
    let mut buffer = LineBuffer::default();
    let lines = buffer.push(Pipe::StdOut, b"one\ntwo\n");
    let past = Instant::now();
    let handled = JournaldLogger::send(&logger.shared, Pipe::StdOut, &lines, Wait::Bounded(past));
    assert_eq!(handled, 2);
    assert!(logger.shared.failing.load(Ordering::Relaxed));
    assert!(received(&socket)?.is_empty());
    Ok(())
}

/// Failing sends get logged once and do not fail the write.
#[tokio::test]
async fn send_failures_are_not_fatal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let mut logger = JournaldLogger::with_socket("test", &path);
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
