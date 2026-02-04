//! File logging functionalities.

use crate::container_io::Pipe;
use anyhow::{Context, Result};
use getset::{CopyGetters, Getters, Setters};
use memchr::memchr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
};
use tracing::{debug, trace};
use tz::{DateTime, TimeZone};

#[derive(Debug, CopyGetters, Getters, Setters)]
/// The main structure used for container log handling.
pub struct CriLogger {
    #[getset(get)]
    /// Path to the file on disk.
    path: PathBuf,

    #[getset(set)]
    /// Open file handle of the `path`.
    file: Option<BufWriter<File>>,

    #[getset(get_copy)]
    /// Maximum allowed log size in bytes.
    max_log_size: Option<usize>,

    #[getset(get_copy, set)]
    /// Current bytes written to the log file.
    bytes_written: usize,

    /// Reusable buffer for line reading to reduce allocations
    line_buf: Vec<u8>,

    /// Cached timestamp string to avoid repeated formatting
    cached_timestamp: String,

    /// Last time the cached timestamp was updated
    last_timestamp_update: Instant,
}

impl CriLogger {
    const ERR_UNINITIALIZED: &'static str = "logger not initialized";

    /// Create a new file logger instance.
    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> Result<CriLogger> {
        Ok(Self {
            path: path.as_ref().into(),
            file: None,
            max_log_size,
            bytes_written: 0,
            line_buf: Vec::with_capacity(256), // Pre-allocate for typical log lines
            cached_timestamp: String::new(),
            last_timestamp_update: Instant::now(),
        })
    }

    /// Asynchronously initialize the CRI logger.
    pub async fn init(&mut self) -> Result<()> {
        debug!("Initializing CRI logger in path {}", self.path().display());
        self.set_file(Self::open(self.path()).await?.into());
        Ok(())
    }

    /// Write the contents of the provided reader into the file logger.
    pub async fn write<T>(&mut self, pipe: Pipe, bytes: T) -> Result<()>
    where
        T: AsyncBufRead + Unpin,
    {
        let mut reader = BufReader::new(bytes);

        // Update cached timestamp if it's been more than 100ms or if empty (first use)
        let now = Instant::now();
        if self.cached_timestamp.is_empty()
            || now.duration_since(self.last_timestamp_update) >= Duration::from_millis(100)
        {
            let local_tz = TimeZone::local().context("get local timezone")?;
            self.cached_timestamp = DateTime::now(local_tz.as_ref())
                .context("get local datetime")?
                .to_string();
            self.last_timestamp_update = now;
        }

        let min_log_len = self
            .cached_timestamp
            .len()
            .checked_add(10) // len of " stdout " + "P "
            .context("min log line len exceeds usize")?;

        loop {
            // Read the line - reuse buffer with clear instead of allocating
            self.line_buf.clear();
            if self.line_buf.capacity() < min_log_len {
                self.line_buf
                    .reserve(min_log_len - self.line_buf.capacity());
            }
            let (read, partial) = Self::read_line(&mut reader, &mut self.line_buf).await?;

            if read == 0 {
                break;
            }

            let mut bytes_to_be_written = read + min_log_len;
            if partial {
                bytes_to_be_written += 1; // the added newline
            }

            let mut new_bytes_written = self
                .bytes_written()
                .checked_add(bytes_to_be_written)
                .unwrap_or_default();

            if new_bytes_written == 0 {
                self.reopen()
                    .await
                    .context("reopen logs because of overflowing bytes_written")?;
            }

            if let Some(max_log_size) = self.max_log_size() {
                trace!(
                    "Verifying log size: max_log_size = {}, bytes_written = {},  bytes_to_be_written = {}, new_bytes_written = {}",
                    max_log_size,
                    self.bytes_written(),
                    bytes_to_be_written,
                    new_bytes_written,
                );

                if new_bytes_written > max_log_size {
                    new_bytes_written = 0;
                    self.reopen()
                        .await
                        .context("reopen logs because of exceeded size")?;
                }
            }

            // Write the timestamp
            let file = self.file.as_mut().context(Self::ERR_UNINITIALIZED)?;
            file.write_all(self.cached_timestamp.as_bytes()).await?;

            // Add the pipe name
            match pipe {
                Pipe::StdOut => file.write_all(b" stdout ").await,
                Pipe::StdErr => file.write_all(b" stderr ").await,
            }?;

            // Output log tag for partial or newline
            if partial {
                file.write_all(b"P ").await?;
            } else {
                file.write_all(b"F ").await?;
            }

            // Output the actual contents
            file.write_all(&self.line_buf).await?;

            // Output a newline for partial
            if partial {
                file.write_all(b"\n").await?;
            }

            self.set_bytes_written(new_bytes_written);
            trace!("Wrote log line of length {}", bytes_to_be_written);
        }

        self.flush().await
    }

    /// Reopen the container log file.
    pub async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen container log {}", self.path().display());
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .get_ref()
            .sync_all()
            .await?;
        self.init().await
    }

    /// Ensures that all content is written to disk.
    pub async fn flush(&mut self) -> Result<()> {
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .flush()
            .await
            .context("flush file writer")
    }

    /// Open the provided path with the default options.
    async fn open<T: AsRef<Path>>(path: T) -> Result<BufWriter<File>> {
        Ok(BufWriter::new(
            OpenOptions::new()
                .create(true)
                .read(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&path)
                .await
                .context(format!("open log file path '{}'", path.as_ref().display()))?,
        ))
    }

    async fn read_line<T>(r: &mut BufReader<T>, buf: &mut Vec<u8>) -> Result<(usize, bool)>
    where
        T: AsyncBufRead + Unpin,
    {
        let (partial, read) = {
            let available = r.fill_buf().await?;
            match memchr(b'\n', available) {
                Some(i) => {
                    buf.extend_from_slice(&available[..=i]);
                    (false, i + 1)
                }
                None => {
                    buf.extend_from_slice(available);
                    (true, available.len())
                }
            }
        };
        r.consume(read);
        Ok((read, partial))
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
