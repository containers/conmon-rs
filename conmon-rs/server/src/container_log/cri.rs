//! File logging functionalities.

use crate::container_io::Pipe;
use anyhow::{Context, Result};
use memchr::memchr;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::{debug, trace};

#[derive(Debug)]
/// The main structure used for container log handling.
pub struct CriLogger {
    /// Path to the file on disk.
    path: PathBuf,

    /// Open file handle of the `path`.
    file: Option<BufWriter<File>>,

    /// Maximum allowed log size in bytes.
    max_log_size: Option<usize>,

    /// Current bytes written to the log file.
    bytes_written: usize,

    /// Reusable buffer for line reading to reduce allocations
    line_buf: Vec<u8>,

    /// Reusable buffer for composing full log lines before writing
    write_buf: Vec<u8>,

    /// Cached timestamp string to avoid repeated formatting
    cached_timestamp: String,

    /// Last time the cached timestamp was updated
    last_timestamp_update: Instant,
}

impl CriLogger {
    fn path(&self) -> &PathBuf {
        &self.path
    }
    fn set_file(&mut self, val: Option<BufWriter<File>>) {
        self.file = val;
    }
    fn max_log_size(&self) -> Option<usize> {
        self.max_log_size
    }
    fn bytes_written(&self) -> usize {
        self.bytes_written
    }
    fn set_bytes_written(&mut self, val: usize) {
        self.bytes_written = val;
    }
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
            line_buf: Vec::with_capacity(256),
            write_buf: Vec::with_capacity(512),
            cached_timestamp: String::new(),
            last_timestamp_update: Instant::now(),
        })
    }

    /// Initialize the CRI logger.
    pub fn init(&mut self) -> Result<()> {
        debug!("Initializing CRI logger in path {}", self.path().display());
        self.set_file(Self::open(self.path())?.into());
        Ok(())
    }

    /// Write the contents of the provided data into the file logger.
    pub fn write(&mut self, pipe: Pipe, data: &[u8]) -> Result<()> {
        let now = Instant::now();
        if self.cached_timestamp.is_empty()
            || now.duration_since(self.last_timestamp_update) >= Duration::from_millis(100)
        {
            let now_dt = OffsetDateTime::now_local()
                .or_else(|_| Ok::<_, time::error::Format>(OffsetDateTime::now_utc()))
                .context("get local datetime")?;
            self.cached_timestamp = now_dt.format(&Rfc3339).context("format datetime")?;
            self.last_timestamp_update = now;
        }

        let pipe_tag = match pipe {
            Pipe::StdOut => &b" stdout "[..],
            Pipe::StdErr => &b" stderr "[..],
        };

        let mut remaining = data;
        loop {
            if remaining.is_empty() {
                break;
            }

            self.line_buf.clear();
            let (partial, consumed) = match memchr(b'\n', remaining) {
                Some(i) => {
                    self.line_buf.extend_from_slice(&remaining[..=i]);
                    (false, i + 1)
                }
                None => {
                    self.line_buf.extend_from_slice(remaining);
                    (true, remaining.len())
                }
            };
            remaining = &remaining[consumed..];

            // Compose the full log line into write_buf
            self.write_buf.clear();
            self.write_buf
                .extend_from_slice(self.cached_timestamp.as_bytes());
            self.write_buf.extend_from_slice(pipe_tag);
            if partial {
                self.write_buf.extend_from_slice(b"P ");
            } else {
                self.write_buf.extend_from_slice(b"F ");
            }
            self.write_buf.extend_from_slice(&self.line_buf);
            if partial {
                self.write_buf.push(b'\n');
            }

            let bytes_to_be_written = self.write_buf.len();

            let mut new_bytes_written = self
                .bytes_written()
                .checked_add(bytes_to_be_written)
                .unwrap_or_default();

            if new_bytes_written == 0 {
                self.reopen()
                    .context("reopen logs because of overflowing bytes_written")?;
            }

            if let Some(max_log_size) = self.max_log_size() {
                trace!(
                    "Verifying log size: max_log_size = {}, bytes_written = {}, bytes_to_be_written = {}, new_bytes_written = {}",
                    max_log_size,
                    self.bytes_written(),
                    bytes_to_be_written,
                    new_bytes_written,
                );

                if new_bytes_written > max_log_size {
                    new_bytes_written = 0;
                    self.reopen()
                        .context("reopen logs because of exceeded size")?;
                }
            }

            let file = self.file.as_mut().context(Self::ERR_UNINITIALIZED)?;
            file.write_all(&self.write_buf)?;

            self.set_bytes_written(new_bytes_written);
            trace!("Wrote log line of length {}", bytes_to_be_written);
        }

        self.flush()
    }

    /// Reopen the container log file.
    pub fn reopen(&mut self) -> Result<()> {
        debug!("Reopen container log {}", self.path().display());
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .get_ref()
            .sync_all()?;
        self.init()
    }

    /// Ensures that all content is written to disk.
    pub fn flush(&mut self) -> Result<()> {
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .flush()
            .context("flush file writer")
    }

    /// Open the provided path with the default options.
    fn open<T: AsRef<Path>>(path: T) -> Result<BufWriter<File>> {
        Ok(BufWriter::new(
            OpenOptions::new()
                .create(true)
                .read(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("open log file path '{}'", path.as_ref().display()))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::NamedTempFile;
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};

    #[test]
    fn write_stdout_success() -> Result<()> {
        let buffer = "this is a line\nand another line\n";

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::new(path, None)?;
        sut.init()?;

        sut.write(Pipe::StdOut, buffer.as_bytes())?;

        let res = fs::read_to_string(path)?;
        assert!(res.contains(" stdout F this is a line"));
        assert!(res.contains(" stdout F and another line"));

        let timestamp = res.split_whitespace().next().context("no timestamp")?;
        OffsetDateTime::parse(timestamp, &Rfc3339).context("unable to parse timestamp")?;
        Ok(())
    }

    #[test]
    fn write_stdout_stderr_success() -> Result<()> {
        let buffer = "a\nb\nc\n";

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::new(path, None)?;
        sut.init()?;

        sut.write(Pipe::StdOut, buffer.as_bytes())?;
        sut.write(Pipe::StdErr, buffer.as_bytes())?;

        let res = fs::read_to_string(path)?;
        assert!(res.contains(" stdout F a"));
        assert!(res.contains(" stdout F b"));
        assert!(res.contains(" stdout F c"));
        assert!(res.contains(" stderr F a"));
        assert!(res.contains(" stderr F b"));
        assert!(res.contains(" stderr F c"));
        Ok(())
    }

    #[test]
    fn write_reopen() -> Result<()> {
        let buffer = "a\nb\nc\nd\ne\nf\n";

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::new(path, Some(150))?;
        sut.init()?;

        sut.write(Pipe::StdOut, buffer.as_bytes())?;

        let res = fs::read_to_string(path)?;
        assert!(!res.contains(" stdout F a"));
        assert!(!res.contains(" stdout F b"));
        assert!(!res.contains(" stdout F c"));
        assert!(res.contains(" stdout F d"));
        assert!(res.contains(" stdout F e"));
        assert!(res.contains(" stdout F f"));
        Ok(())
    }

    #[test]
    fn write_multi_reopen() -> Result<()> {
        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::new(path, Some(150))?;
        sut.init()?;

        sut.write(Pipe::StdOut, "abcd\nabcd\nabcd\n".as_bytes())?;
        sut.write(Pipe::StdErr, "a\nb\nc\n".as_bytes())?;

        let res = fs::read_to_string(path)?;
        assert!(!res.contains(" stdout "));
        assert!(res.contains(" stderr F a"));
        assert!(res.contains(" stderr F b"));
        assert!(res.contains(" stderr F c"));
        Ok(())
    }

    #[test]
    fn init_failure() -> Result<()> {
        let mut sut = CriLogger::new("/file/does/not/exist", None)?;
        assert!(sut.init().is_err());
        Ok(())
    }
}
