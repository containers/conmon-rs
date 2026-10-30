//! The logger of the CRI log format.

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
            // Created readable by the group as well, like by conmon.
            file: LogFile::new(path, Some(0o640), max_log_size),
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

        // The first error, the lines after a failed one still get written.
        let mut res = Ok(());
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
            res = res.and(self.file.commit_entry(start).await);
        }

        res.and(self.file.end_chunk().await)
    }
}

#[cfg(test)]
mod tests;
