//! File logging functionalities.

use crate::{
    container_io::Pipe,
    container_log::{LogFile, Logger, lines},
};
use anyhow::{Context, Result};
use std::{fmt::Write, path::Path};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tracing::debug;

#[derive(Debug)]
/// The main structure used for container log handling.
pub struct CriLogger {
    /// The log file on disk.
    file: LogFile,

    /// The timestamp of the entries of the current chunk.
    timestamp: Timestamp,
}

impl CriLogger {
    /// Create a new file logger instance.
    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> CriLogger {
        Self {
            file: LogFile::new(path, Some(0o600), max_log_size),
            timestamp: Timestamp::default(),
        }
    }

    /// Use a fixed timestamp for every entry, so that tests can compare the
    /// written bytes.
    #[cfg(test)]
    pub(crate) fn freeze_timestamp(&mut self, timestamp: &str) {
        self.timestamp.formatted = timestamp.into();
        self.timestamp.frozen = true;
    }
}

/// The RFC 3339 timestamp of the time a chunk got read, with nanoseconds like
/// the ones of conmon. The local date, time of day and UTC offset only change
/// with the second, they get formatted once per second, the fraction for
/// every chunk.
#[derive(Debug, Default)]
struct Timestamp {
    /// The second since the epoch the cached parts belong to.
    second: Option<i64>,

    /// The local date and time of day of that second, like
    /// `2026-10-20T12:00:00`.
    date_time: String,

    /// The UTC offset of that second, like `+02:00` or `Z`.
    offset: String,

    /// The whole timestamp of the current chunk.
    formatted: String,

    #[cfg(test)]
    frozen: bool,
}

impl Timestamp {
    /// The length of the date and time of day of an RFC 3339 timestamp.
    const DATE_TIME_LEN: usize = "2026-10-20T12:00:00".len();

    /// Take the current time and return its timestamp.
    fn update(&mut self) -> Result<&str> {
        #[cfg(test)]
        if self.frozen {
            return Ok(&self.formatted);
        }
        let now = OffsetDateTime::now_utc();
        let second = now.unix_timestamp();
        if self.second != Some(second) {
            let offset = UtcOffset::local_offset_at(now).unwrap_or(UtcOffset::UTC);
            // Without a fraction, the date and time of day are followed by
            // the offset.
            let whole = now
                .to_offset(offset)
                .replace_nanosecond(0)
                .context("strip nanoseconds")?
                .format(&Rfc3339)
                .context("format datetime")?;
            let (date_time, offset) = whole
                .split_at_checked(Self::DATE_TIME_LEN)
                .context("split datetime")?;
            self.date_time = date_time.into();
            self.offset = offset.into();
            self.second = Some(second);
        }
        self.formatted.clear();
        write!(
            self.formatted,
            "{}.{:09}{}",
            self.date_time,
            now.nanosecond(),
            self.offset
        )
        .context("format timestamp")?;
        Ok(&self.formatted)
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
        let timestamp = self.timestamp.update()?;

        let pipe_tag: &[u8] = match pipe {
            Pipe::StdOut => b" stdout ",
            Pipe::StdErr => b" stderr ",
        };

        for (line, partial) in lines(bytes) {
            // Assemble the whole line into the chunk buffer
            let buf = self.file.buf();
            let start = buf.len();
            buf.extend_from_slice(timestamp.as_bytes());
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
    async fn timestamps_are_taken_per_chunk() -> Result<()> {
        let mut sut = Timestamp::default();
        let first = OffsetDateTime::parse(sut.update()?, &Rfc3339)?;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let second = OffsetDateTime::parse(sut.update()?, &Rfc3339)?;
        assert!(second > first, "{first} {second}");
        // Nine digits of the fraction like conmon.
        let fraction = sut.update()?[Timestamp::DATE_TIME_LEN..]
            .strip_prefix('.')
            .context("fraction")?
            .chars()
            .take_while(char::is_ascii_digit)
            .count();
        assert_eq!(fraction, 9);
        assert!((OffsetDateTime::now_utc() - second).whole_seconds() < 5);
        Ok(())
    }

    #[tokio::test]
    async fn init_failure() -> Result<()> {
        let mut sut = CriLogger::new("/file/does/not/exist", None);
        assert!(sut.init().await.is_err());
        Ok(())
    }
}
