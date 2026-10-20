use crate::{
    container_io::Pipe,
    container_log::{Line, Logger},
};
use anyhow::{Context, Result};
use libsystemd::logging::{Priority, journal_send};
use std::sync::Arc;
use tokio::task;
use tracing::{debug, warn};

/// Writes the container output to the journal with the container ID fields
/// and priorities conmon uses, so that `journalctl CONTAINER_ID=<id>` finds
/// it and podman can put partial lines back together.
#[derive(Debug)]
pub struct JournaldLogger {
    /// The fields attached to every entry. Shared with the blocking tasks
    /// which send the entries.
    fields: Arc<[(&'static str, String)]>,
}

impl JournaldLogger {
    /// The length of the short container ID field.
    const SHORT_ID_LEN: usize = 12;

    /// The field marking a line which had no ending yet.
    const PARTIAL_FIELD: (&'static str, &'static str) = ("CONTAINER_PARTIAL_MESSAGE", "true");

    pub fn new(id: &str) -> Self {
        Self {
            fields: Arc::new([
                ("CONTAINER_ID_FULL", id.to_owned()),
                (
                    "CONTAINER_ID",
                    id.chars().take(Self::SHORT_ID_LEN).collect(),
                ),
                ("SYSLOG_IDENTIFIER", "conmonrs".into()),
            ]),
        }
    }

    /// The journal priority of the output of a pipe.
    fn priority(pipe: Pipe) -> Priority {
        match pipe {
            Pipe::StdOut => Priority::Info,
            Pipe::StdErr => Priority::Error,
        }
    }

    /// Send the lines to the journal. A journal which is unavailable, for
    /// example while it restarts, must not end the output of the container,
    /// so failures are only logged, once per chunk.
    fn send(fields: &[(&'static str, String)], pipe: Pipe, lines: &[Line]) {
        let priority = Self::priority(pipe);
        let mut failed = 0;
        let mut last_error = None;
        for line in lines {
            let fields = fields
                .iter()
                .map(|(k, v)| (*k, v.as_str()))
                .chain(line.partial.then_some(Self::PARTIAL_FIELD));
            if let Err(e) = journal_send(priority, &String::from_utf8_lossy(&line.text), fields) {
                failed += 1;
                last_error = Some(e);
            }
        }
        if let Some(e) = last_error {
            warn!("Unable to write {failed} lines to the journal, dropping them: {e}");
        }
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

    /// Sending is blocking socket I/O, so the whole chunk is handed to a
    /// single blocking task instead of one per line. The lines share their
    /// buffer, moving them is free.
    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line]) -> Result<()> {
        let fields = self.fields.clone();
        let lines = lines.to_vec();
        task::spawn_blocking(move || Self::send(&fields, pipe, &lines))
            .await
            .context("journal spawn_blocking")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_log::LineBuffer;

    #[test]
    fn fields_and_priorities() {
        let logger = JournaldLogger::new("0123456789abcdef");
        assert!(
            logger
                .fields
                .contains(&("CONTAINER_ID_FULL", "0123456789abcdef".into()))
        );
        assert!(
            logger
                .fields
                .contains(&("CONTAINER_ID", "0123456789ab".into()))
        );
        assert_eq!(
            JournaldLogger::priority(Pipe::StdOut) as u8,
            Priority::Info as u8
        );
        assert_eq!(
            JournaldLogger::priority(Pipe::StdErr) as u8,
            Priority::Error as u8
        );
    }

    /// Writes to the journal of the host if there is one, a missing journal
    /// only gets logged.
    #[tokio::test]
    async fn write_lines_to_journal() -> Result<()> {
        let mut logger = JournaldLogger::new("test");
        logger.init().await?;

        let mut buffer = LineBuffer::default();
        let lines = buffer.push(Pipe::StdOut, b"Test log message\nand another");
        logger.write_lines(Pipe::StdOut, &lines).await?;
        logger.reopen().await?;
        let lines = buffer.finish(Pipe::StdOut);
        assert!(lines[0].partial);
        logger.write_lines(Pipe::StdErr, &lines).await
    }
}
