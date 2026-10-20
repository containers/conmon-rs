use crate::{
    container_io::Pipe,
    container_log::{Line, LogFile, Logger},
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{path::Path, time::SystemTime};
use tracing::debug;

/// A single line of output as JSON. The schema is documented in usage.md.
#[derive(Serialize)]
struct LogEntry<'a> {
    /// Seconds since the Unix epoch.
    timestamp: u64,

    /// `stdout` or `stderr`.
    pipe: &'a str,

    /// The line without its ending.
    message: &'a str,

    /// Whether the line had no ending yet, see [`Line`].
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    partial: bool,
}

#[derive(Debug)]
pub struct JsonLogger {
    /// The log file on disk.
    file: LogFile,
}

impl JsonLogger {
    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> JsonLogger {
        Self {
            file: LogFile::new(path, None, max_log_size),
        }
    }
}

impl Logger for JsonLogger {
    const BUFFERS_LINES: bool = true;

    async fn init(&mut self) -> Result<()> {
        debug!(
            "Initializing JSON logger in path {}",
            self.file.path().display()
        );
        self.file.open().await
    }

    async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen JSON log {}", self.file.path().display());
        self.file.reopen().await
    }

    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line]) -> Result<()> {
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        for line in lines {
            let message = String::from_utf8_lossy(&line.text);
            let log_entry = LogEntry {
                timestamp,
                pipe: pipe.as_str(),
                message: &message,
                partial: line.partial,
            };

            let buf = &mut self.file.buf;
            let start = buf.len();
            if let Err(e) = serde_json::to_writer(&mut *buf, &log_entry) {
                buf.truncate(start);
                return Err(e).context("serialize log entry");
            }
            buf.push(b'\n');
            self.file.commit_entry(start).await?;
        }

        // Write the whole chunk at once instead of per line
        self.file.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_log::LineBuffer;
    use std::fs;
    use tempfile::NamedTempFile;

    /// Write a chunk the way `ContainerLog` does.
    async fn write(logger: &mut JsonLogger, pipe: Pipe, bytes: &[u8]) -> Result<()> {
        let mut buffer = LineBuffer::default();
        let mut lines = buffer.push(pipe, bytes);
        lines.extend(buffer.finish(pipe));
        logger.write_lines(pipe, &lines).await
    }

    #[tokio::test]
    async fn test_json_logger_new() -> Result<()> {
        let logger = JsonLogger::new("/tmp/test.log", Some(1000));
        assert_eq!(
            logger.file.path().to_str().context("path")?,
            "/tmp/test.log"
        );
        assert_eq!(logger.file.max_size, Some(1000));
        Ok(())
    }

    #[tokio::test]
    async fn test_json_logger_init() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut logger = JsonLogger::new(file.path(), Some(1000));
        logger.init().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_json_logger_write() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut logger = JsonLogger::new(file.path(), Some(1000));
        logger.init().await?;

        write(&mut logger, Pipe::StdOut, b"Test log message\nsecond").await?;

        let contents = fs::read_to_string(file.path())?;
        let mut lines = contents.lines();
        let first = lines.next().context("no first line")?;
        assert!(first.contains(r#""pipe":"stdout""#));
        assert!(first.contains(r#""message":"Test log message""#));
        let second = lines.next().context("no second line")?;
        assert!(second.contains(r#""message":"second""#));
        assert!(lines.next().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_json_logger_line_endings() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut logger = JsonLogger::new(file.path(), None);
        logger.init().await?;

        write(
            &mut logger,
            Pipe::StdErr,
            b"terminal\r\n  indented \nno newline",
        )
        .await?;

        let contents = fs::read_to_string(file.path())?;
        let mut lines = contents.lines();
        assert!(
            lines
                .next()
                .context("no line")?
                .contains(r#""message":"terminal"}"#)
        );
        assert!(
            lines
                .next()
                .context("no line")?
                .contains(r#""message":"  indented "}"#)
        );
        assert!(
            lines
                .next()
                .context("no line")?
                .contains(r#""message":"no newline","partial":true}"#)
        );
        assert!(lines.next().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_json_logger_reopen() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut logger = JsonLogger::new(file.path(), Some(1000));
        logger.init().await?;

        write(
            &mut logger,
            Pipe::StdOut,
            b"Test log message before reopen\n",
        )
        .await?;
        logger.reopen().await?;
        write(
            &mut logger,
            Pipe::StdOut,
            b"Test log message after reopen\n",
        )
        .await?;

        let contents = fs::read_to_string(file.path())?;
        assert!(contents.contains("Test log message after reopen"));
        assert!(!contents.contains("Test log message before reopen"));
        Ok(())
    }

    #[tokio::test]
    async fn test_json_logger_rotation_counts_trigger() -> Result<()> {
        let file = NamedTempFile::new()?;
        // Every entry is about 60 bytes, so the third one triggers a rotation.
        let mut logger = JsonLogger::new(file.path(), Some(150));
        logger.init().await?;

        write(&mut logger, Pipe::StdOut, b"a\nb\nc\n").await?;

        let contents = fs::read_to_string(file.path())?;
        assert_eq!(logger.file.bytes_written, contents.len());
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains(r#""message":"c""#));
        Ok(())
    }
}
