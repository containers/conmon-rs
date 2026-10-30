use crate::{
    container_io::Pipe,
    container_log::{Line, LogFile, Logger},
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{path::Path, time::SystemTime};
use tracing::debug;

/// A single line of output as JSON. The schema is documented in usage.md.
#[derive(Serialize)]
struct LogEntry<'a> {
    /// Seconds since the Unix epoch.
    timestamp: u64,

    /// `stdout` or `stderr`.
    pipe: &'a str,

    /// The line without its ending.
    message: &'a str,

    /// Whether the line had no ending yet, see [`Line`].
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    partial: bool,
}

#[derive(Debug)]
pub struct JsonLogger {
    /// The log file on disk.
    file: LogFile,
}

impl JsonLogger {
    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> JsonLogger {
        Self {
            // Created readable by the owner only, like the CRI log.
            file: LogFile::new(path, Some(0o600), max_log_size),
        }
    }
}

impl Logger for JsonLogger {
    const BUFFERS_LINES: bool = true;

    async fn init(&mut self) -> Result<()> {
        debug!(
            "Initializing JSON logger in path {}",
            self.file.path().display()
        );
        self.file.open().await
    }

    async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen JSON log {}", self.file.path().display());
        self.file.reopen().await
    }

    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line<'_>]) -> Result<()> {
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // The first error, the lines after a failed one still get written.
        let mut res = Ok(());
        for line in lines {
            let message = String::from_utf8_lossy(&line.text);
            let log_entry = LogEntry {
                timestamp,
                pipe: pipe.as_str(),
                message: &message,
                partial: line.partial,
            };

            let buf = self.file.buf();
            let start = buf.len();
            if let Err(e) = serde_json::to_writer(&mut *buf, &log_entry) {
                buf.truncate(start);
                res = res.and(Err(e).context("serialize log entry"));
                continue;
            }
            buf.push(b'\n');
            res = res.and(self.file.commit_entry(start).await);
        }

        // Write the whole chunk at once instead of per line
        res.and(self.file.end_chunk().await)
    }
}

#[cfg(test)]
mod tests;
