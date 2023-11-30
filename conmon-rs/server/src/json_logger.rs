use crate::container_io::Pipe;
use anyhow::{Context, Result};
use getset::{CopyGetters, Getters, Setters};
use memchr::memchr;
use std::{
    marker::Unpin,
    path::{Path, PathBuf},
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
};
use tracing::{debug, trace};
use tz::{DateTime, TimeZone};

// File logger implementation.
#[derive(Debug, CopyGetters, Getters, Setters)]
// The main structure used for container log handling.
pub struct JsonLogger {
    #[getset(get)]
    // Path to the file on disk.
    path: PathBuf,

    #[getset(set)]
    // Open file handle of the `path`.
    file: Option<BufWriter<File>>,

    #[getset(get_copy)]
    // Maximum allowed log size in bytes.
    max_log_size: Option<usize>,

    #[getset(get_copy, set)]
    // Current bytes written to the log file.
    bytes_written: usize,
}

impl JsonLogger {
    const ERR_UNINITIALIZED: &'static str = "logger not initialized";

    // Create a new file logger instance.
    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> Result<JsonLogger> {
        Ok(Self {
            path: path.as_ref().into(),
            file: None,
            max_log_size,
            bytes_written: 0,
        })
    }

    // Asynchronously initialize the CRI logger.
    pub async fn init(&mut self) -> Result<()> {
        debug!("Initializing JSON logger in path {}", self.path().display());
        self.set_file(Self::open(self.path()).await?.into());
        Ok(())
    }

    //Reopen the container log file.
    pub async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen container log {}", self.path().display());
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .get_ref()
            .sync_all()
            .await?;
        self.init().await
    }
  
    // Ensures that all content is written to disk.
    pub async fn flush(&mut self) -> Result<()> {
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .flush()
            .await
            .context("flush file writer")
    }

 
    // Write the contents of the provided reader into the log file.
    pub async fn write<T>(&mut self, pipe: Pipe, bytes: T) -> Result<()>
    where
        T: AsyncBufRead + Unpin + Copy,
    {
        let mut buf = Vec::new();
        let mut partial = false;
        let mut read = 0;
        while !partial {
            let (r, p) = Self::read_line(&mut buf, bytes).await?;
            read += r;
            partial = p;
        }
        self.bytes_written += read;
        if let Some(max_log_size) = self.max_log_size {
            if self.bytes_written > max_log_size {
                self.reopen().await?;
            }
        }
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .write_all(&buf)
            .await
            .context("write to file")
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

 
