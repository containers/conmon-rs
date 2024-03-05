use crate::container_io::Pipe;
use crate::journal::Journal;
use anyhow::Result;
use getset::{Getters, Setters};
use std::io::Write;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tracing::debug;

#[derive(Debug, Getters, Setters)]
pub struct JournaldLogger {
    #[getset(get_copy)]
    max_log_size: Option<usize>,

    #[getset(get_copy, set)]
    bytes_written: usize,
}

impl JournaldLogger {
    pub fn new(max_log_size: Option<usize>) -> Result<Self> {
        Ok(Self {
            max_log_size,
            bytes_written: 0,
        })
    }

    pub async fn init(&mut self) -> Result<()> {
        debug!("Initializing Journald logger");
        Ok(())
    }

    pub async fn write<T>(&mut self, pipe: Pipe, mut bytes: T) -> Result<()>
    where
        T: AsyncBufRead + Unpin,
    {
        let mut line_buf = String::new();
        while bytes.read_line(&mut line_buf).await? > 0 {
            let log_entry = format!(
                "{:?} [{}] {}",
                std::time::SystemTime::now(),
                match pipe {
                    Pipe::StdOut => "stdout",
                    Pipe::StdErr => "stderr",
                },
                line_buf.trim()
            );

            let bytes_len = log_entry.len();
            self.bytes_written += bytes_len;

            if let Some(max_size) = self.max_log_size {
                if self.bytes_written > max_size {
                    self.reopen().await?;
                    self.bytes_written = 0;
                }
            }

            Journal.write_all(log_entry.as_bytes())?;
            Journal.flush()?;
            line_buf.clear();
        }

        Ok(())
    }

    pub async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen Journald log");
        // Implement logic for reopening if necessary
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_journald_logger_new() {
        let logger = JournaldLogger::new(Some(1000)).unwrap();
        assert_eq!(logger.max_log_size.unwrap(), 1000);
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
