use crate::container_io::Pipe;
use anyhow::{Context, Result};
use memchr::memchr;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
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

    /// Reusable buffer for line reading to reduce allocations
    line_buf: Vec<u8>,

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
            line_buf: Vec::with_capacity(256),
            json_buf: Vec::with_capacity(512),
        })
    }

    pub fn init(&mut self) -> Result<()> {
        debug!("Initializing JSON logger in path {}", self.path().display());
        self.set_file(Self::open(self.path())?.into());
        Ok(())
    }

    pub fn write(&mut self, pipe: Pipe, data: &[u8]) -> Result<()> {
        let mut remaining = data;
        loop {
            self.line_buf.clear();
            let consumed = match memchr(b'\n', remaining) {
                Some(i) => {
                    self.line_buf.extend_from_slice(&remaining[..=i]);
                    i + 1
                }
                None if !remaining.is_empty() => {
                    self.line_buf.extend_from_slice(remaining);
                    remaining.len()
                }
                None => break,
            };
            remaining = &remaining[consumed..];

            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let pipe_str = match pipe {
                Pipe::StdOut => "stdout",
                Pipe::StdErr => "stderr",
            };

            let message = String::from_utf8_lossy(&self.line_buf);
            let message_trimmed = message.trim();

            let log_entry = LogEntry {
                timestamp,
                pipe: pipe_str,
                message: message_trimmed,
            };

            self.json_buf.clear();
            serde_json::to_writer(&mut self.json_buf, &log_entry).context("serialize log entry")?;

            self.bytes_written += self.json_buf.len() + 1; // +1 for newline

            #[allow(clippy::collapsible_if)]
            if let Some(max_size) = self.max_log_size {
                if self.bytes_written > max_size {
                    self.reopen()?;
                    self.bytes_written = 0;
                }
            }

            let file = self.file.as_mut().context(Self::ERR_UNINITIALIZED)?;
            file.write_all(&self.json_buf)?;
            file.write_all(b"\n")?;
        }

        self.flush()?;
        Ok(())
    }

    pub fn reopen(&mut self) -> Result<()> {
        debug!("Reopen JSON log {}", self.path().display());
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .get_ref()
            .sync_all()?;
        self.init()
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .flush()
            .context("flush file writer")
    }

    fn open<T: AsRef<Path>>(path: T) -> Result<BufWriter<File>> {
        Ok(BufWriter::new(
            OpenOptions::new()
                .create(true)
                .read(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("open log file path '{}'", path.as_ref().display()))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_json_logger_new() {
        let logger = JsonLogger::new("/tmp/test.log", Some(1000)).unwrap();
        assert_eq!(logger.path().to_str().unwrap(), "/tmp/test.log");
        assert_eq!(logger.max_log_size.unwrap(), 1000);
    }

    #[test]
    fn test_json_logger_init() {
        let mut logger = JsonLogger::new("/tmp/test_init.log", Some(1000)).unwrap();
        logger.init().unwrap();
        assert!(logger.file.is_some());
    }

    #[test]
    fn test_json_logger_write() {
        let mut logger = JsonLogger::new("/tmp/test_write.log", Some(1000)).unwrap();
        logger.init().unwrap();

        logger.write(Pipe::StdOut, b"Test log message\n").unwrap();

        let contents = fs::read_to_string("/tmp/test_write.log").unwrap();
        assert!(contents.contains("Test log message"));
    }

    #[test]
    fn test_json_logger_reopen() {
        let mut logger = JsonLogger::new("/tmp/test_reopen.log", Some(1000)).unwrap();
        logger.init().unwrap();

        logger
            .write(Pipe::StdOut, b"Test log message before reopen\n")
            .unwrap();

        logger.reopen().unwrap();

        logger
            .write(Pipe::StdOut, b"Test log message after reopen\n")
            .unwrap();

        let contents = fs::read_to_string("/tmp/test_reopen.log").unwrap();
        assert!(contents.contains("Test log message after reopen"));
        assert!(!contents.contains("Test log message before reopen"));
    }
}
