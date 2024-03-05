pub use crate::container_io::Pipe;
use anyhow::{Context, Result};
use getset::{CopyGetters, Getters, Setters};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
};
use tracing::debug;

#[derive(Debug, CopyGetters, Getters, Setters)]
pub struct JsonLogger {
    #[getset(get)]
    path: PathBuf,

    #[getset(set)]
    file: Option<BufWriter<File>>,

    #[getset(get_copy)]
    max_log_size: Option<usize>,

    #[getset(get_copy, set)]
    bytes_written: usize,
}

impl JsonLogger {
    const ERR_UNINITIALIZED: &'static str = "logger not initialized";

    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> Result<JsonLogger> {
        Ok(Self {
            path: path.as_ref().into(),
            file: None,
            max_log_size,
            bytes_written: 0,
        })
    }

    pub async fn init(&mut self) -> Result<()> {
        debug!("Initializing JSON logger in path {}", self.path().display());
        self.set_file(Self::open(self.path()).await?.into());
        Ok(())
    }

    pub async fn write<T>(&mut self, pipe: Pipe, bytes: T) -> Result<()>
    where
        T: AsyncBufRead + Unpin,
    {
        let mut reader = BufReader::new(bytes);
        let mut line_buf = Vec::new();

        while reader.read_until(b'\n', &mut line_buf).await? > 0 {
            let log_entry = json!({
                "timestamp": format!("{:?}", std::time::SystemTime::now()),
                "pipe": match pipe {
                    Pipe::StdOut => "stdout",
                    Pipe::StdErr => "stderr",
                },
                "message": String::from_utf8_lossy(&line_buf).trim().to_string()
            });

            let log_str = log_entry.to_string();
            let bytes = log_str.as_bytes();
            self.bytes_written += bytes.len();

            if let Some(max_size) = self.max_log_size {
                if self.bytes_written > max_size {
                    self.reopen().await?;
                    self.bytes_written = 0;
                }
            }

            let file = self.file.as_mut().context(Self::ERR_UNINITIALIZED)?;
            file.write_all(bytes).await?;
            file.write_all(b"\n").await?;
            self.flush().await?;
            line_buf.clear();
        }

        Ok(())
    }

    pub async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen JSON log {}", self.path().display());
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .get_ref()
            .sync_all()
            .await?;
        self.init().await
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .flush()
            .await
            .context("flush file writer")
    }

    async fn open<T: AsRef<Path>>(path: T) -> Result<BufWriter<File>> {
        Ok(BufWriter::new(
            OpenOptions::new()
                .create(true)
                .read(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .await
                .context(format!("open log file path '{}'", path.as_ref().display()))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn test_json_logger_new() {
        let logger = JsonLogger::new("/tmp/test.log", Some(1000)).unwrap();
        assert_eq!(logger.path().to_str().unwrap(), "/tmp/test.log");
        assert_eq!(logger.max_log_size().unwrap(), 1000);
    }

    #[tokio::test]
    async fn test_json_logger_init() {
        let mut logger = JsonLogger::new("/tmp/test_init.log", Some(1000)).unwrap();
        logger.init().await.unwrap();
        assert!(logger.file.is_some());
    }

    #[tokio::test]
    async fn test_json_logger_write() {
        let mut logger = JsonLogger::new("/tmp/test_write.log", Some(1000)).unwrap();
        logger.init().await.unwrap();

        let cursor = Cursor::new(b"Test log message\n".to_vec());
        logger.write(Pipe::StdOut, cursor).await.unwrap();

        // Read back from the file
        let mut file = File::open("/tmp/test_write.log").await.unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).await.unwrap();

        // Check if the file contains the logged message
        assert!(contents.contains("Test log message"));
    }

    #[tokio::test]
    async fn test_json_logger_reopen() {
        let mut logger = JsonLogger::new("/tmp/test_reopen.log", Some(1000)).unwrap();
        logger.init().await.unwrap();

        // Write to the file
        let cursor = Cursor::new(b"Test log message before reopen\n".to_vec());
        logger.write(Pipe::StdOut, cursor).await.unwrap();

        // Reopen the file
        logger.reopen().await.unwrap();

        // Write to the file again
        let cursor = Cursor::new(b"Test log message after reopen\n".to_vec());
        logger.write(Pipe::StdOut, cursor).await.unwrap();

        // Read back from the file
        let mut file = File::open("/tmp/test_reopen.log").await.unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).await.unwrap();

        // Check if the file contains the logged message
        assert!(contents.contains("Test log message after reopen"));
        assert!(!contents.contains("Test log message before reopen"));
    }
}
