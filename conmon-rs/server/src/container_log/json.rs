use crate::{container_io::Pipe, container_log::LogFile};
use anyhow::{Context, Result};
use memchr::memchr;
use serde::Serialize;
use std::{path::Path, time::SystemTime};
use tracing::debug;

/// Log entry structure for JSON serialization
#[derive(Serialize)]
struct LogEntry<'a> {
    timestamp: u64, // Unix timestamp in seconds
    pipe: &'a str,
    message: &'a str,
}

#[derive(Debug)]
pub struct JsonLogger {
    /// The log file on disk.
    file: LogFile,
    max_log_size: Option<usize>,
    bytes_written: usize,

    /// Reusable buffer for serializing a single entry
    json_buf: Vec<u8>,
}

impl JsonLogger {
    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> Result<JsonLogger> {
        Ok(Self {
            file: LogFile::new(path, None),
            max_log_size,
            bytes_written: 0,
            json_buf: Vec::with_capacity(512), // Pre-allocate for JSON output
        })
    }

    pub async fn init(&mut self) -> Result<()> {
        debug!(
            "Initializing JSON logger in path {}",
            self.file.path().display()
        );
        self.file.open().await
    }

    pub async fn write(&mut self, pipe: Pipe, bytes: &[u8]) -> Result<()> {
        // Get Unix timestamp in seconds (more efficient than Debug format)
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let pipe_str = match pipe {
            Pipe::StdOut => "stdout",
            Pipe::StdErr => "stderr",
        };

        let mut pos = 0;
        while pos < bytes.len() {
            // Find the next newline or end of buffer
            let end = memchr(b'\n', &bytes[pos..])
                .map(|i| pos + i + 1)
                .unwrap_or(bytes.len());
            let line = &bytes[pos..end];
            pos = end;

            // Convert message to UTF-8, trim whitespace
            let message = String::from_utf8_lossy(line);

            let log_entry = LogEntry {
                timestamp,
                pipe: pipe_str,
                message: message.trim(),
            };

            // Serialize the entry and its trailing newline into the reusable
            // buffer first, so that the size check happens before it gets
            // appended to the chunk.
            self.json_buf.clear();
            serde_json::to_writer(&mut self.json_buf, &log_entry).context("serialize log entry")?;
            self.json_buf.push(b'\n');

            let total = self.bytes_written.saturating_add(self.json_buf.len());
            if self.max_log_size.is_some_and(|max| total > max) {
                self.reopen().await?;
                // The entry which triggered the rotation ends up in the new
                // file and counts towards its size.
                self.bytes_written = self.json_buf.len();
            } else {
                self.bytes_written = total;
            }

            self.file.buf.extend_from_slice(&self.json_buf);
        }

        // Write the whole chunk at once instead of per line
        self.file.flush().await
    }

    pub async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen JSON log {}", self.file.path().display());
        self.file.reopen().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_json_logger_new() -> Result<()> {
        let logger = JsonLogger::new("/tmp/test.log", Some(1000))?;
        assert_eq!(
            logger.file.path().to_str().context("path")?,
            "/tmp/test.log"
        );
        assert_eq!(logger.max_log_size, Some(1000));
        Ok(())
    }

    #[tokio::test]
    async fn test_json_logger_init() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut logger = JsonLogger::new(file.path(), Some(1000))?;
        logger.init().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_json_logger_write() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut logger = JsonLogger::new(file.path(), Some(1000))?;
        logger.init().await?;

        logger
            .write(Pipe::StdOut, b"Test log message\nsecond")
            .await?;

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
    async fn test_json_logger_reopen() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut logger = JsonLogger::new(file.path(), Some(1000))?;
        logger.init().await?;

        logger
            .write(Pipe::StdOut, b"Test log message before reopen\n")
            .await?;
        logger.reopen().await?;
        logger
            .write(Pipe::StdOut, b"Test log message after reopen\n")
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
        let mut logger = JsonLogger::new(file.path(), Some(150))?;
        logger.init().await?;

        logger.write(Pipe::StdOut, b"a\nb\nc\n").await?;
        assert_eq!(logger.bytes_written, logger.json_buf.len());

        let contents = fs::read_to_string(file.path())?;
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains(r#""message":"c""#));
        Ok(())
    }
}
