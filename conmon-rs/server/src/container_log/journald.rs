use crate::{
    container_io::Pipe,
    container_log::{Line, Logger},
    journal::{self, Entry, Priority, Wait},
};
use anyhow::{Context, Result};
use std::{io::ErrorKind, path::Path, sync::Arc};
use tokio::task;
use tracing::{debug, warn};

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

    pub fn new(id: &str) -> Self {
        let short_id: String = id.chars().take(Self::SHORT_ID_LEN).collect();
        let mut fields = Entry::default();
        fields
            .field("CONTAINER_ID_FULL", id)
            .field("CONTAINER_ID", short_id)
            .field("SYSLOG_IDENTIFIER", "conmonrs");
        Self {
            shared: Arc::new(Shared {
                fields,
                #[cfg(test)]
                socket: None,
            }),
        }
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
    /// only logged, once per chunk. Without a journal, or with one which did
    /// not take a line in time, the rest of the chunk is dropped right away
    /// instead of failing the same way line by line. Without waiting, the
    /// sending stops at the first line which the journal does not take right
    /// away.
    fn send(shared: &Shared, pipe: Pipe, lines: &[Line], wait: Wait) -> usize {
        let priority = Self::priority(pipe);
        let mut entry = Entry::default();
        let mut handled = lines.len();
        let mut failed = 0;
        let mut last_error = None;
        for (i, line) in lines.iter().enumerate() {
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
        if let Some(e) = last_error {
            warn!("Unable to write {failed} lines to the journal, dropping them: {e}");
        }
        handled
    }
}

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
        task::spawn_blocking(move || Self::send(&shared, pipe, &rest, Wait::Bounded))
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
            let mut logger = Self::new(id);
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
    fn short_ids_are_kept() {
        let logger = JournaldLogger::new("abc");
        assert_eq!(
            decode(logger.shared.fields.as_bytes()),
            [
                field("CONTAINER_ID_FULL", "abc"),
                field("CONTAINER_ID", "abc"),
                field("SYSLOG_IDENTIFIER", "conmonrs"),
            ]
        );
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

    /// Failing sends get logged and do not fail the write.
    #[tokio::test]
    async fn send_failures_are_not_fatal() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut logger = JournaldLogger::with_socket("test", &dir.path().join("missing"));
        let mut buffer = LineBuffer::default();
        let lines = buffer.push(Pipe::StdOut, b"lost\n");
        logger.write_lines(Pipe::StdOut, &lines).await
    }
}
