//! File logging functionalities.

use crate::{
    container_io::Pipe,
    container_log::{LogFile, Logger, lines},
};
use anyhow::{Context, Result};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::debug;

/// How long a formatted timestamp is reused before it gets refreshed.
const TIMESTAMP_CACHE_DURATION: Duration = Duration::from_millis(100);

#[derive(Debug)]
/// The main structure used for container log handling.
pub struct CriLogger {
    /// The log file on disk.
    file: LogFile,

    /// Cached timestamp string to avoid repeated formatting
    cached_timestamp: String,

    /// Last time the cached timestamp was updated
    last_timestamp_update: Instant,
}

impl CriLogger {
    /// Create a new file logger instance.
    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> CriLogger {
        Self {
            file: LogFile::new(path, Some(0o600), max_log_size),
            cached_timestamp: String::new(),
            last_timestamp_update: Instant::now(),
        }
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
}

impl Logger for CriLogger {
    /// Partial lines are written right away with the `P` marker.
    const BUFFERS_LINES: bool = false;

    async fn init(&mut self) -> Result<()> {
        debug!(
            "Initializing CRI logger in path {}",
            self.file.path().display()
        );
        self.file.open().await
    }

    async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen container log {}", self.file.path().display());
        self.file.reopen().await
    }

    async fn write_chunk(&mut self, pipe: Pipe, bytes: &[u8]) -> Result<()> {
        self.update_timestamp()?;

        let pipe_tag: &[u8] = match pipe {
            Pipe::StdOut => b" stdout ",
            Pipe::StdErr => b" stderr ",
        };

        for (line, partial) in lines(bytes) {
            // Assemble the whole line into the chunk buffer
            let buf = &mut self.file.buf;
            let start = buf.len();
            buf.extend_from_slice(self.cached_timestamp.as_bytes());
            buf.extend_from_slice(pipe_tag);
            buf.extend_from_slice(if partial { b"P " } else { b"F " });
            buf.extend_from_slice(line);
            if partial {
                buf.push(b'\n');
            }
            self.file.commit_entry(start).await?;
        }

        self.file.flush().await
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
        let mut sut = CriLogger::new(path, None);
        sut.init().await?;

        sut.write_chunk(Pipe::StdOut, bytes).await?;

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
        let mut sut = CriLogger::new(path, None);
        sut.init().await?;

        sut.write_chunk(Pipe::StdOut, b"full\npartial").await?;

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
        let mut sut = CriLogger::new(path, None);
        sut.init().await?;

        sut.write_chunk(Pipe::StdOut, bytes1).await?;
        sut.write_chunk(Pipe::StdErr, bytes2).await?;

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
        let mut sut = CriLogger::new(path, Some(150));
        sut.init().await?;

        sut.write_chunk(Pipe::StdOut, bytes).await?;

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
        let mut sut = CriLogger::new(path, Some(150));
        sut.init().await?;

        sut.write_chunk(Pipe::StdOut, "abcd\nabcd\nabcd\n".as_bytes())
            .await?;
        sut.write_chunk(Pipe::StdErr, "a\nb\nc\n".as_bytes())
            .await?;

        let res = fs::read_to_string(path)?;
        assert!(!res.contains(" stdout "));
        assert!(res.contains(" stderr F a"));
        assert!(res.contains(" stderr F b"));
        assert!(res.contains(" stderr F c"));
        Ok(())
    }

    #[tokio::test]
    async fn init_failure() -> Result<()> {
        let mut sut = CriLogger::new("/file/does/not/exist", None);
        assert!(sut.init().await.is_err());
        Ok(())
    }
}
