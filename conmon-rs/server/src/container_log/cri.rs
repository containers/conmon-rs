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

    /// Use a fixed timestamp for every entry, so that tests can compare the
    /// written bytes. The refresh never happens because the last update lies
    /// in the future, which `duration_since` saturates to zero.
    #[cfg(test)]
    pub(crate) fn freeze_timestamp(&mut self, timestamp: &str) {
        self.cached_timestamp = timestamp.into();
        self.last_timestamp_update = Instant::now() + Duration::from_secs(24 * 60 * 60);
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
    use crate::container_log::test_helpers::{TIMESTAMP, cri_logger, read};
    use anyhow::Context;
    use tempfile::NamedTempFile;
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};

    /// A CRI log entry with the frozen timestamp.
    fn entry(pipe: &str, tag: &str, line: &str) -> String {
        format!("{TIMESTAMP} {pipe} {tag} {line}\n")
    }

    #[tokio::test]
    async fn write_stdout_success() -> Result<()> {
        let (file, mut sut) = cri_logger(None).await?;

        sut.write_chunk(Pipe::StdOut, b"this is a line\nand another line\n")
            .await?;

        assert_eq!(
            read(&file)?,
            entry("stdout", "F", "this is a line") + &entry("stdout", "F", "and another line")
        );
        Ok(())
    }

    #[tokio::test]
    async fn write_partial_line() -> Result<()> {
        let (file, mut sut) = cri_logger(None).await?;

        sut.write_chunk(Pipe::StdOut, b"full\npartial").await?;

        assert_eq!(
            read(&file)?,
            entry("stdout", "F", "full") + &entry("stdout", "P", "partial")
        );
        Ok(())
    }

    #[tokio::test]
    async fn write_stdout_stderr_success() -> Result<()> {
        let (file, mut sut) = cri_logger(None).await?;

        sut.write_chunk(Pipe::StdOut, b"a\nb\n").await?;
        sut.write_chunk(Pipe::StdErr, b"c\n").await?;

        assert_eq!(
            read(&file)?,
            entry("stdout", "F", "a") + &entry("stdout", "F", "b") + &entry("stderr", "F", "c")
        );
        Ok(())
    }

    #[tokio::test]
    async fn rotate_on_exceeded_size() -> Result<()> {
        // Room for exactly three entries, the fourth one starts a new file.
        let max_size = 3 * entry("stdout", "F", "a").len();
        let (file, mut sut) = cri_logger(Some(max_size)).await?;

        sut.write_chunk(Pipe::StdOut, b"a\nb\nc\nd\ne\nf\n").await?;

        assert_eq!(
            read(&file)?,
            entry("stdout", "F", "d") + &entry("stdout", "F", "e") + &entry("stdout", "F", "f")
        );
        Ok(())
    }

    #[tokio::test]
    async fn rotate_on_exceeded_size_across_chunks() -> Result<()> {
        // The first chunk fills the file exactly, the first entry of the
        // second one starts a new file.
        let max_size = 3 * entry("stdout", "F", "abcd").len();
        let (file, mut sut) = cri_logger(Some(max_size)).await?;

        sut.write_chunk(Pipe::StdOut, b"abcd\nabcd\nabcd\n").await?;
        assert_eq!(read(&file)?.len(), max_size);
        sut.write_chunk(Pipe::StdErr, b"a\nb\nc\n").await?;

        assert_eq!(
            read(&file)?,
            entry("stderr", "F", "a") + &entry("stderr", "F", "b") + &entry("stderr", "F", "c")
        );
        Ok(())
    }

    #[tokio::test]
    async fn timestamp_is_rfc3339() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut sut = CriLogger::new(file.path(), None);
        sut.init().await?;

        sut.write_chunk(Pipe::StdOut, b"line\n").await?;

        let res = read(&file)?;
        let timestamp = res.split_whitespace().next().context("no timestamp")?;
        OffsetDateTime::parse(timestamp, &Rfc3339).context("unable to parse timestamp")?;
        Ok(())
    }

    #[tokio::test]
    async fn init_failure() -> Result<()> {
        let mut sut = CriLogger::new("/file/does/not/exist", None);
        assert!(sut.init().await.is_err());
        Ok(())
    }
}
