use crate::{container_io::Pipe, journal::Journal};
use anyhow::{Context, Result};
use std::io::Write;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
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

    pub async fn write<T>(&mut self, _: Pipe, mut bytes: T) -> Result<()>
    where
        T: AsyncBufRead + Unpin,
    {
        let mut line_buf = String::new();
        while bytes.read_line(&mut line_buf).await? > 0 {
            Journal
                .write_all(line_buf.as_bytes())
                .context("write to journal")?;
            line_buf.clear();
        }

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
    use std::io::Cursor;

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

        let cursor = Cursor::new(b"Test log message\n".to_vec());
        assert!(logger.write(Pipe::StdOut, cursor).await.is_ok());

        // Verifying the actual log message in Journald might require additional setup or permissions.
    }

    #[tokio::test]
    async fn test_journald_logger_reopen() {
        let mut logger = JournaldLogger::new(Some(1000)).unwrap();
        logger.init().await.unwrap();

        let cursor = Cursor::new(b"Test log message before reopen\n".to_vec());
        assert!(logger.write(Pipe::StdOut, cursor).await.is_ok());

        assert!(logger.reopen().await.is_ok());

        let cursor = Cursor::new(b"Test log message after reopen\n".to_vec());
        assert!(logger.write(Pipe::StdOut, cursor).await.is_ok());

        // As with the write test, verifying the actual log messages in Journald might require additional setup or permissions.
    }
}
