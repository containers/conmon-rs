use crate::{
    container_io::Pipe,
    container_log::{Line, Logger},
    journal::{self, Entry, Priority, Wait},
};
use anyhow::{Context, Result, bail};
use std::{
    io::{self, ErrorKind},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::task;
use tracing::{debug, info, warn};

/// Writes the container output to the journal with the container ID fields
/// and priorities conmon uses, so that `journalctl CONTAINER_ID=<id>` finds
/// it and podman can put partial lines back together.
#[derive(Debug)]
pub struct JournaldLogger {
    /// Shared with the blocking tasks which send the entries.
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    /// The fields of every entry, encoded once.
    fields: Entry,
    /// Whether the last send failed, so that a journal which is unavailable
    /// gets reported once and not for every chunk of output.
    failing: AtomicBool,
    /// A temporary socket instead of the one of journald.
    #[cfg(test)]
    socket: Option<std::path::PathBuf>,
}

impl Shared {
    fn socket(&self) -> &Path {
        #[cfg(test)]
        if let Some(socket) = &self.socket {
            return socket;
        }
        Path::new(journal::SOCKET_PATH)
    }
}

impl JournaldLogger {
    /// The length of the short container ID field.
    const SHORT_ID_LEN: usize = 12;

    /// The field marking a line which had no ending yet.
    const PARTIAL_FIELD: (&'static str, &'static str) = ("CONTAINER_PARTIAL_MESSAGE", "true");

    /// The fields the logger sets itself, which a label cannot replace.
    const OWN_FIELDS: [&'static str; 8] = [
        "MESSAGE",
        "PRIORITY",
        "CONTAINER_ID",
        "CONTAINER_ID_FULL",
        "CONTAINER_NAME",
        "CONTAINER_TAG",
        "SYSLOG_IDENTIFIER",
        Self::PARTIAL_FIELD.0,
    ];

    /// The most labels of a logger. journald drops entries with more than
    /// 1024 fields (`ENTRY_FIELDS_MAX`), which would lose every line.
    const MAX_LABELS: usize = 256;

    /// The most bytes of the names and values of all labels together, which
    /// every entry carries.
    const MAX_LABEL_BYTES: usize = 64 * 1024;

    /// A logger with the fields conmon writes: the container ID, the name
    /// and the tag if given, which is the syslog identifier as well, and the
    /// labels.
    pub fn new<'a>(
        id: &str,
        name: &str,
        tag: &str,
        labels: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self> {
        let short_id: String = id.chars().take(Self::SHORT_ID_LEN).collect();
        let mut fields = Entry::default();
        fields
            .field("CONTAINER_ID_FULL", id)
            .field("CONTAINER_ID", short_id);
        if !name.is_empty() {
            fields.field("CONTAINER_NAME", name);
        }
        if tag.is_empty() {
            fields.field("SYSLOG_IDENTIFIER", journal::IDENTIFIER);
        } else {
            fields
                .field("CONTAINER_TAG", tag)
                .field("SYSLOG_IDENTIFIER", tag);
        }
        let (mut count, mut bytes) = (0, 0);
        for (key, value) in labels {
            Self::validate_label(key)?;
            count += 1;
            bytes += key.len() + value.len();
            if count > Self::MAX_LABELS || bytes > Self::MAX_LABEL_BYTES {
                bail!(
                    "too many journald labels: at most {} with {} bytes together",
                    Self::MAX_LABELS,
                    Self::MAX_LABEL_BYTES
                );
            }
            fields.field(key, value);
        }
        Ok(Self {
            shared: Arc::new(Shared {
                fields,
                failing: AtomicBool::new(false),
                #[cfg(test)]
                socket: None,
            }),
        })
    }

    /// Check that a label is a field name which the journal takes and which
    /// the logger does not set itself.
    fn validate_label(key: &str) -> Result<()> {
        if !Entry::valid_key(key) {
            bail!(
                "invalid journald label {key:?}: use uppercase letters, digits and \
                 underscores, starting with a letter, at most 64 characters"
            );
        }
        if Self::OWN_FIELDS.contains(&key) {
            bail!("journald label {key:?} replaces a field of the logger");
        }
        Ok(())
    }

    /// The journal priority of the output of a pipe.
    fn priority(pipe: Pipe) -> Priority {
        match pipe {
            Pipe::StdOut => Priority::Info,
            Pipe::StdErr => Priority::Error,
        }
    }

    /// Send the lines to the journal and return how many of them it took
    /// or got dropped. A journal which is unavailable, for example while it
    /// restarts, must not end the output of the container, so failures are
    /// only logged, once until a send succeeds again. Without a journal, or
    /// with one which did not take a line in time, the rest of the chunk is
    /// dropped right away instead of failing the same way line by line.
    /// Without waiting, the sending stops at the first line which the
    /// journal does not take right away. Waiting, the lines which did not
    /// go out by the deadline of the wait get dropped, a journal which takes
    /// each line just in time would block the log of the container for
    /// minutes per chunk otherwise.
    fn send(shared: &Shared, pipe: Pipe, lines: &[Line], wait: Wait) -> usize {
        let priority = Self::priority(pipe);
        let mut entry = Entry::default();
        let mut handled = lines.len();
        let mut failed = 0;
        let mut last_error = None;
        for (i, line) in lines.iter().enumerate() {
            if let Wait::Bounded(deadline) = wait
                && Instant::now() >= deadline
            {
                failed += lines.len() - i;
                last_error = Some(io::Error::new(
                    ErrorKind::TimedOut,
                    format!("journal did not take the chunk within {CHUNK_TIMEOUT:?}"),
                ));
                break;
            }
            entry.reset(priority, &line.text).extend(&shared.fields);
            if line.partial {
                entry.field(Self::PARTIAL_FIELD.0, Self::PARTIAL_FIELD.1);
            }
            let Err(e) = journal::send_at(wait, shared.socket(), &entry) else {
                continue;
            };
            if wait == Wait::Never && e.kind() == ErrorKind::WouldBlock {
                handled = i;
                break;
            }
            let rest = journal::unavailable(&e) || e.kind() == ErrorKind::TimedOut;
            last_error = Some(e);
            if rest {
                failed += lines.len() - i;
                break;
            }
            failed += 1;
        }
        match last_error {
            Some(e) if !shared.failing.swap(true, Ordering::Relaxed) => warn!(
                "Unable to write {failed} lines to the journal, dropping lines \
                 until it takes them again: {e}"
            ),
            Some(e) => debug!("Unable to write {failed} lines to the journal: {e}"),
            None if handled > 0 && shared.failing.swap(false, Ordering::Relaxed) => {
                info!("The journal takes lines again");
            }
            None => {}
        }
        handled
    }
}

/// How long the lines of a chunk which the journal did not take right away
/// may wait for it in total. A line which started before it may take one
/// more `journal::SEND_TIMEOUT`, sends which a signal interrupted get
/// retried only until it.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(1);

impl Logger for JournaldLogger {
    const BUFFERS_LINES: bool = true;

    async fn init(&mut self) -> Result<()> {
        debug!("Initializing journald logger");
        Ok(())
    }

    async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen journald log");
        Ok(())
    }

    /// A journal which keeps up takes the lines right away. The ones it does
    /// not take are sent from a single blocking task, which waits for it a
    /// bounded time, instead of one per line. The lines share their buffer,
    /// moving them is free.
    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line]) -> Result<()> {
        let sent = Self::send(&self.shared, pipe, lines, Wait::Never);
        let Some(rest) = lines.get(sent..).filter(|rest| !rest.is_empty()) else {
            return Ok(());
        };
        let shared = self.shared.clone();
        let rest = rest.to_vec();
        // From when a thread of the blocking pool took it, a busy pool must
        // not drop the chunk without trying.
        task::spawn_blocking(move || {
            let deadline = Instant::now() + CHUNK_TIMEOUT;
            Self::send(&shared, pipe, &rest, Wait::Bounded(deadline))
        })
        .await
        .map(drop)
        .context("journal spawn_blocking")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{container_log::LineBuffer, journal::tests::decode};
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
                field("SYSLOG_IDENTIFIER", "conmonrs"),
            ]
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
                field("SYSLOG_IDENTIFIER", "conmonrs"),
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
        let handled =
            JournaldLogger::send(&logger.shared, Pipe::StdOut, &lines, Wait::Bounded(past));
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
}
