use crate::container_io::Pipe;
use anyhow::{Context, Result};
use bytes::Bytes;
use libsystemd::logging::{Priority, journal_print};
use tokio::task;
use tracing::debug;

#[derive(Debug)]
pub struct JournaldLogger;

impl JournaldLogger {
    pub fn new(_: Option<usize>) -> Result<Self> {
        Ok(Self)
    }

    pub async fn init(&mut self) -> Result<()> {
        debug!("Initializing journald logger");
        Ok(())
    }

    /// Write every line of the provided bytes as a journal entry.
    ///
    /// Sending to the journal is blocking socket I/O, so the whole chunk is
    /// handed to a single blocking task instead of one per line. The bytes are
    /// reference counted, which makes moving them into the task free.
    pub async fn write(&mut self, _: Pipe, bytes: Bytes) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        task::spawn_blocking(move || {
            for line in bytes.split_inclusive(|&b| b == b'\n') {
                journal_print(Priority::Notice, &String::from_utf8_lossy(line))
                    .context("write to journal")?;
            }
            Ok(())
        })
        .await
        .context("journal spawn_blocking")?
    }

    pub async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen journald log");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_journald_logger_new() {
        JournaldLogger::new(Some(1000)).unwrap();
    }

    #[tokio::test]
    async fn test_journald_logger_init() {
        let mut logger = JournaldLogger::new(Some(1000)).unwrap();
        assert!(logger.init().await.is_ok());
    }

    #[tokio::test]
    async fn test_journald_logger_write() {
        let mut logger = JournaldLogger::new(Some(1000)).unwrap();
        logger.init().await.unwrap();

        let data = Bytes::from_static(b"Test log message\nand another one\n");
        assert!(logger.write(Pipe::StdOut, data).await.is_ok());
    }

    #[tokio::test]
    async fn test_journald_logger_reopen() {
        let mut logger = JournaldLogger::new(Some(1000)).unwrap();
        logger.init().await.unwrap();

        let data1 = Bytes::from_static(b"Test log message before reopen\n");
        assert!(logger.write(Pipe::StdOut, data1).await.is_ok());

        assert!(logger.reopen().await.is_ok());

        let data2 = Bytes::from_static(b"Test log message after reopen\n");
        assert!(logger.write(Pipe::StdOut, data2).await.is_ok());
    }
}
