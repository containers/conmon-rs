//! File logging functionalities.

use crate::{container_io::Pipe, container_log::LogFile};
use anyhow::{Context, Result};
use memchr::memchr;
use std::{
    path::Path,
    time::{Duration, Instant},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::{debug, trace};

/// How long a formatted timestamp is reused before it gets refreshed.
const TIMESTAMP_CACHE_DURATION: Duration = Duration::from_millis(100);

#[derive(Debug)]
/// The main structure used for container log handling.
pub struct CriLogger {
    /// The log file on disk.
    file: LogFile,

    /// Maximum allowed log size in bytes.
    max_log_size: Option<usize>,

    /// Current bytes written to the log file.
    bytes_written: usize,

    /// Cached timestamp string to avoid repeated formatting
    cached_timestamp: String,

    /// Last time the cached timestamp was updated
    last_timestamp_update: Instant,
}

impl CriLogger {
    /// Create a new file logger instance.
    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> Result<CriLogger> {
        Ok(Self {
            file: LogFile::new(path, Some(0o600)),
            max_log_size,
            bytes_written: 0,
            cached_timestamp: String::new(),
            last_timestamp_update: Instant::now(),
        })
    }

    /// Asynchronously initialize the CRI logger.
    pub async fn init(&mut self) -> Result<()> {
        debug!(
            "Initializing CRI logger in path {}",
            self.file.path().display()
        );
        self.file.open().await
    }

    /// Refresh the cached timestamp if it is empty or older than
    /// `TIMESTAMP_CACHE_DURATION`.
    fn update_timestamp(&mut self) -> Result<()> {
        let now = Instant::now();
        if self.cached_timestamp.is_empty()
            || now.duration_since(self.last_timestamp_update) >= TIMESTAMP_CACHE_DURATION
        {
            let now_dt = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
            self.cached_timestamp = now_dt.format(&Rfc3339).context("format datetime")?;
            self.last_timestamp_update = now;
        }
        Ok(())
    }

    /// Write the contents of the provided bytes into the file logger.
    pub async fn write(&mut self, pipe: Pipe, bytes: &[u8]) -> Result<()> {
        self.update_timestamp()?;

        let pipe_tag: &[u8] = match pipe {
            Pipe::StdOut => b" stdout ",
            Pipe::StdErr => b" stderr ",
        };

        // The timestamp, the pipe tag and the "P " or "F " marker.
        let min_log_len = self
            .cached_timestamp
            .len()
            .checked_add(pipe_tag.len() + 2)
            .context("min log line len exceeds usize")?;

        let mut pos = 0;
        while pos < bytes.len() {
            // Find the next newline or end of buffer
            let (end, partial) = match memchr(b'\n', &bytes[pos..]) {
                Some(i) => (pos + i + 1, false), // Found newline, include it, not partial
                None => (bytes.len(), true),     // No newline, partial line
            };
            let line = &bytes[pos..end];
            pos = end;

            let mut bytes_to_be_written = line.len() + min_log_len;
            if partial {
                bytes_to_be_written += 1; // the added newline
            }

            let total = self.bytes_written.saturating_add(bytes_to_be_written);
            trace!(
                "Verifying log size: max_log_size = {:?}, bytes_written = {}, bytes_to_be_written = {}",
                self.max_log_size, self.bytes_written, bytes_to_be_written,
            );

            let new_bytes_written = if self.max_log_size.is_some_and(|max| total > max) {
                self.reopen()
                    .await
                    .context("reopen logs because of exceeded size")?;
                // The line which triggered the rotation ends up in the new file
                // and counts towards its size.
                bytes_to_be_written
            } else {
                total
            };

            // Assemble the whole line into the chunk buffer
            let buf = &mut self.file.buf;
            buf.extend_from_slice(self.cached_timestamp.as_bytes());
            buf.extend_from_slice(pipe_tag);
            buf.extend_from_slice(if partial { b"P " } else { b"F " });
            buf.extend_from_slice(line);
            if partial {
                buf.push(b'\n');
            }

            self.bytes_written = new_bytes_written;
            trace!("Wrote log line of length {}", bytes_to_be_written);
        }

        self.file.flush().await
    }

    /// Reopen the container log file.
    pub async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen container log {}", self.file.path().display());
        self.file.reopen().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::NamedTempFile;
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};

    #[tokio::test]
    async fn write_stdout_success() -> Result<()> {
        let buffer = "this is a line\nand another line\n";
        let bytes = buffer.as_bytes();

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::new(path, None)?;
        sut.init().await?;

        sut.write(Pipe::StdOut, bytes).await?;

        let res = fs::read_to_string(path)?;
        assert!(res.contains(" stdout F this is a line"));
        assert!(res.contains(" stdout F and another line"));

        let timestamp = res.split_whitespace().next().context("no timestamp")?;
        OffsetDateTime::parse(timestamp, &Rfc3339).context("unable to parse timestamp")?;
        Ok(())
    }

    #[tokio::test]
    async fn write_partial_line() -> Result<()> {
        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::new(path, None)?;
        sut.init().await?;

        sut.write(Pipe::StdOut, b"full\npartial").await?;

        let res = fs::read_to_string(path)?;
        assert!(res.contains(" stdout F full\n"));
        assert!(res.ends_with(" stdout P partial\n"));
        Ok(())
    }

    #[tokio::test]
    async fn write_stdout_stderr_success() -> Result<()> {
        let buffer = "a\nb\nc\n";
        let bytes1 = buffer.as_bytes();
        let bytes2 = buffer.as_bytes();

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::new(path, None)?;
        sut.init().await?;

        sut.write(Pipe::StdOut, bytes1).await?;
        sut.write(Pipe::StdErr, bytes2).await?;

        let res = fs::read_to_string(path)?;
        assert!(res.contains(" stdout F a"));
        assert!(res.contains(" stdout F b"));
        assert!(res.contains(" stdout F c"));
        assert!(res.contains(" stderr F a"));
        assert!(res.contains(" stderr F b"));
        assert!(res.contains(" stderr F c"));
        Ok(())
    }

    #[tokio::test]
    async fn write_reopen() -> Result<()> {
        let buffer = "a\nb\nc\nd\ne\nf\n";
        let bytes = buffer.as_bytes();

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::new(path, Some(150))?;
        sut.init().await?;

        sut.write(Pipe::StdOut, bytes).await?;

        let res = fs::read_to_string(path)?;
        assert!(!res.contains(" stdout F a"));
        assert!(!res.contains(" stdout F b"));
        assert!(!res.contains(" stdout F c"));
        assert!(res.contains(" stdout F d"));
        assert!(res.contains(" stdout F e"));
        assert!(res.contains(" stdout F f"));
        Ok(())
    }

    #[tokio::test]
    async fn write_multi_reopen() -> Result<()> {
        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::new(path, Some(150))?;
        sut.init().await?;

        sut.write(Pipe::StdOut, "abcd\nabcd\nabcd\n".as_bytes())
            .await?;
        sut.write(Pipe::StdErr, "a\nb\nc\n".as_bytes()).await?;

        let res = fs::read_to_string(path)?;
        assert!(!res.contains(" stdout "));
        assert!(res.contains(" stderr F a"));
        assert!(res.contains(" stderr F b"));
        assert!(res.contains(" stderr F c"));
        Ok(())
    }

    #[tokio::test]
    async fn init_failure() -> Result<()> {
        let mut sut = CriLogger::new("/file/does/not/exist", None)?;
        assert!(sut.init().await.is_err());
        Ok(())
    }
}
