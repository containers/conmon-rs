use crate::{container_io::Pipe, journal::Journal};
use anyhow::{Context, Result};
use std::io::Write;
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

    pub async fn write(&mut self, _: Pipe, bytes: &[u8]) -> Result<()> {
        // Convert to owned Vec to move into spawn_blocking
        let bytes_owned = bytes.to_vec();
        task::spawn_blocking(move || {
            Journal
                .write_all(&bytes_owned)
                .context("write to journal")
        })
        .await
        .context("journal spawn_blocking")??;
        Ok(())
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

        let data = b"Test log message\n";
        assert!(logger.write(Pipe::StdOut, data).await.is_ok());
    }

    #[tokio::test]
    async fn test_journald_logger_reopen() {
        let mut logger = JournaldLogger::new(Some(1000)).unwrap();
        logger.init().await.unwrap();

        let data1 = b"Test log message before reopen\n";
        assert!(logger.write(Pipe::StdOut, data1).await.is_ok());

        assert!(logger.reopen().await.is_ok());

        let data2 = b"Test log message after reopen\n";
        assert!(logger.write(Pipe::StdOut, data2).await.is_ok());
    }
}
