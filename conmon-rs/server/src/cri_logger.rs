//! File logging functionalities.

#![allow(dead_code)] // TODO: remove me when actually used

use anyhow::{Context, Result};
use chrono::offset::Local;
use getset::{CopyGetters, Getters, MutGetters, Setters};
use log::{debug, trace};
use memchr::memchr;
use std::{
    marker::Unpin,
    path::{Path, PathBuf},
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
};

#[derive(Debug, CopyGetters, Getters, MutGetters, Setters)]
/// The main structure used for container log handling.
pub struct CriLogger {
    #[getset(get)]
    /// Path to the file on disk.
    path: PathBuf,

    #[getset(get, get_mut, set)]
    /// Open file handle of the `path`.
    file: BufWriter<File>,

    #[getset(get_copy)]
    /// Maximum allowed log size in bytes.
    max_log_size: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
/// Available pipe types.
pub enum Pipe {
    /// Standard output.
    StdOut,

    /// Standard error.
    StdErr,
}

impl CriLogger {
    /// Create a new file logger instance.
    pub async fn from<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> Result<Self> {
        Ok(Self {
            path: path.as_ref().into(),
            file: Self::open(&path).await?,
            max_log_size,
        })
    }

    /// Write the contents of the provided reader into the file logger. Ensure to manually call
    /// `flush` if you need the written log data on disk.
    pub async fn write<T>(&mut self, pipe: Pipe, reader: &mut BufReader<T>) -> Result<()>
    where
        T: AsyncBufRead + Unpin,
    {
        // Get the RFC3339 timestmap
        let timestamp = Local::now().to_rfc3339();
        let min_log_len = timestamp
            .len()
            .checked_add(10) // len of " stdout " + "P "
            .context("min log line len exceeds usize")?;
        let mut bytes_written = 0;

        loop {
            // Read the line
            let mut line_buf = Vec::with_capacity(min_log_len);
            let (read, partial) = Self::read_line(reader, &mut line_buf).await?;

            if read == 0 {
                break;
            }

            let mut bytes_to_be_written = read + min_log_len;
            if partial {
                bytes_to_be_written += 1; // the added newline
            }

            if let Some(max_log_size) = self.max_log_size() {
                trace!(
                    "Verifying log size: max_log_size = {}, bytes_written = {}, bytes_to_be_written = {}", 
                    max_log_size, bytes_written, bytes_to_be_written,
                );
                if (bytes_written + bytes_to_be_written) > max_log_size {
                    bytes_written = 0;
                    self.reopen()
                        .await
                        .context("reopen logs because of exceeded size")?;
                }
            }

            // Write the timestmap
            self.file_mut().write_all(timestamp.as_bytes()).await?;

            // Add the pipe name
            match pipe {
                Pipe::StdOut => self.file_mut().write_all(b" stdout ").await,
                Pipe::StdErr => self.file_mut().write_all(b" stderr ").await,
            }?;

            // Output log tag for partial or newline
            if partial {
                self.file_mut().write_all(b"P ").await?;
            } else {
                self.file_mut().write_all(b"F ").await?;
            }

            // Output the actual contents
            self.file_mut().write_all(&line_buf).await?;

            // Output a newline for partial
            if partial {
                self.file_mut().write_all(b"\n").await?;
            }

            bytes_written += bytes_to_be_written;
            trace!("Wrote log line of length {}", bytes_to_be_written);
        }

        Ok(())
    }

    /// Reopen the container log file.
    pub async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen container log {}", self.path().display());
        self.file().get_ref().sync_all().await?;
        self.set_file(Self::open(self.path()).await?);
        Ok(())
    }

    /// Ensures that all content is written to disk.
    pub async fn flush(&mut self) -> Result<()> {
        self.file_mut().flush().await.context("flush file writer")
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
                .context("open log file path")?,
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
    use chrono::DateTime;
    use std::fs;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn write_stdout_success() -> Result<()> {
        let buffer = "this is a line\nand another line\n";
        let mut reader = BufReader::new(buffer.as_bytes());

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::from(path, None).await?;

        sut.write(Pipe::StdOut, &mut reader).await?;
        sut.flush().await?;

        let res = fs::read_to_string(path)?;
        assert!(res.contains(" stdout F this is a line"));
        assert!(res.contains(" stdout F and another line"));

        DateTime::parse_from_rfc3339(res.split_whitespace().next().context("no timestamp")?)
            .context("unable to parse timestamp")?;
        Ok(())
    }

    #[tokio::test]
    async fn write_stdout_stderr_success() -> Result<()> {
        let buffer = "a\nb\nc\n";
        let mut reader1 = BufReader::new(buffer.as_bytes());
        let mut reader2 = BufReader::new(buffer.as_bytes());

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::from(path, None).await?;

        sut.write(Pipe::StdOut, &mut reader1).await?;
        sut.write(Pipe::StdErr, &mut reader2).await?;
        sut.flush().await?;

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
        let mut reader = BufReader::new(buffer.as_bytes());

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = CriLogger::from(path, Some(150)).await?;

        sut.write(Pipe::StdOut, &mut reader).await?;
        sut.flush().await?;

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
    async fn from_failure() {
        let res = CriLogger::from("/file/does/not/exist", None).await;
        assert!(res.is_err())
    }
}
