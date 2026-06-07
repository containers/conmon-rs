use crate::container_io::Pipe;
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
};
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
    path: PathBuf,
    file: Option<BufWriter<File>>,
    max_log_size: Option<usize>,
    bytes_written: usize,

    /// Reusable buffer for JSON serialization
    json_buf: Vec<u8>,
}

impl JsonLogger {
    fn path(&self) -> &PathBuf {
        &self.path
    }
    fn set_file(&mut self, val: Option<BufWriter<File>>) {
        self.file = val;
    }
}

impl JsonLogger {
    const ERR_UNINITIALIZED: &'static str = "logger not initialized";

    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> Result<JsonLogger> {
        Ok(Self {
            path: path.as_ref().into(),
            file: None,
            max_log_size,
            bytes_written: 0,
            json_buf: Vec::with_capacity(512), // Pre-allocate for JSON output
        })
    }

    pub async fn init(&mut self) -> Result<()> {
        debug!("Initializing JSON logger in path {}", self.path().display());
        self.set_file(Self::open(self.path()).await?.into());
        Ok(())
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
            let end = bytes[pos..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| pos + i + 1)
                .unwrap_or(bytes.len());

            let line = &bytes[pos..end];

            // Convert message to UTF-8, trim whitespace
            let message = String::from_utf8_lossy(line);
            let message_trimmed = message.trim();

            // Create log entry struct
            let log_entry = LogEntry {
                timestamp,
                pipe: pipe_str,
                message: message_trimmed,
            };

            // Serialize directly to reusable buffer to avoid String allocation
            self.json_buf.clear();
            serde_json::to_writer(&mut self.json_buf, &log_entry).context("serialize log entry")?;

            self.bytes_written += self.json_buf.len() + 1; // +1 for newline

            #[allow(clippy::collapsible_if)]
            if let Some(max_size) = self.max_log_size {
                if self.bytes_written > max_size {
                    self.reopen().await?;
                    self.bytes_written = 0;
                }
            }

            let file = self.file.as_mut().context(Self::ERR_UNINITIALIZED)?;
            file.write_all(&self.json_buf).await?;
            file.write_all(b"\n").await?;

            pos = end;
        }

        // Flush once at the end instead of per-line to reduce syscall overhead
        self.flush().await?;
        Ok(())
    }

    pub async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen JSON log {}", self.path().display());
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .get_ref()
            .sync_all()
            .await?;
        self.init().await
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .flush()
            .await
            .context("flush file writer")
    }

    async fn open<T: AsRef<Path>>(path: T) -> Result<BufWriter<File>> {
        Ok(BufWriter::new(
            OpenOptions::new()
                .create(true)
                .read(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .await
                .with_context(|| format!("open log file path '{}'", path.as_ref().display()))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn test_json_logger_new() {
        let logger = JsonLogger::new("/tmp/test.log", Some(1000)).unwrap();
        assert_eq!(logger.path().to_str().unwrap(), "/tmp/test.log");
        assert_eq!(logger.max_log_size.unwrap(), 1000);
    }

    #[tokio::test]
    async fn test_json_logger_init() {
        let mut logger = JsonLogger::new("/tmp/test_init.log", Some(1000)).unwrap();
        logger.init().await.unwrap();
        assert!(logger.file.is_some());
    }

    #[tokio::test]
    async fn test_json_logger_write() {
        let mut logger = JsonLogger::new("/tmp/test_write.log", Some(1000)).unwrap();
        logger.init().await.unwrap();

        let data = b"Test log message\n";
        logger.write(Pipe::StdOut, data).await.unwrap();

        // Read back from the file
        let mut file = File::open("/tmp/test_write.log").await.unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).await.unwrap();

        // Check if the file contains the logged message
        assert!(contents.contains("Test log message"));
    }

    #[tokio::test]
    async fn test_json_logger_reopen() {
        let mut logger = JsonLogger::new("/tmp/test_reopen.log", Some(1000)).unwrap();
        logger.init().await.unwrap();

        // Write to the file
        let data1 = b"Test log message before reopen\n";
        logger.write(Pipe::StdOut, data1).await.unwrap();

        // Reopen the file
        logger.reopen().await.unwrap();

        // Write to the file again
        let data2 = b"Test log message after reopen\n";
        logger.write(Pipe::StdOut, data2).await.unwrap();

        // Read back from the file
        let mut file = File::open("/tmp/test_reopen.log").await.unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).await.unwrap();

        // Check if the file contains the logged message
        assert!(contents.contains("Test log message after reopen"));
        assert!(!contents.contains("Test log message before reopen"));
    }
}
