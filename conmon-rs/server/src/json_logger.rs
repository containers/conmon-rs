//JSON logger 
//module imports begin 
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

#[derive(Debug, CopyGetters, Getters, Setters)]

pub struct JSONLogger {
    #[getset(get)] // path to the file.
    path: PathBuf,

    #[getset(set)] // open file handle of the path.
    file: Option<BufWriter<File>>,

    #[getset(get_copy)] // maximum allowed log size in bytes.
    max_log_size: Option<usize>,

    #[getset(get_copy, set)] // current bytes written to the log file.
    bytes_written: usize,
}

impl JSONLogger{
    const ERR_UNINITIALIZED: &'static str = "logger not initialized";

    //Creation of a new instance.
    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> Result<CriLogger> {
        Ok(Self {
            path: path.as_ref().into(),
            file: None,
            max_log_size,
            bytes_written: 0,
        })
    }

    //We use the asynchronous initialization of our JSONLogger
    pub async fn init(&mut self) -> Result<()> {
        let log_path = self.path();
        //log_path is the path to the file.
        log::debug!("Initializing JSONLogger in path {}", log_path.display());
        
        let file = Self::open(log_path).await?;
        self.set_file(file.into());
        
        Ok(())
    }

    //Writing the reader contents into the file logger.
    pub async fn write<T>(&mut self,pipe: Pipe, bytes:T) -> Result<()>
    where 
    T: AsyncBufRead + Unpin,
    {
//reader variable 
let mut reader = BufReader::new(bytes);


//Get the timestamp for the log output first.
let local_tz = TimeZone::local().context("get local timezone")?;
let timestamp = DateTime::now(local_tz.as_ref())
    .format("%Y-%m-%dT%H:%M:%S.%f%:z")
    .to_string();
let min_log_len = timestamp
    .len()
    .checked_add(2)
    .context("timestamp length overflow")?;

//looping through the reader
loop {
    //Read the line
    let mut line_buf = Vec::with_capacity(min_log_len);
    let (read, partial) = Self::read_line(&mut reader, &mut line_buf).await?;

    if read == 0 {
        break;
    }

    //Write the line
    let mut file = self.file().ok_or_else(|| Self::ERR_UNINITIALIZED)?;
    file.write_all(timestamp.as_bytes()).await?;
    file.write_all(b" ").await?;
    file.write_all(pipe.as_bytes()).await?;
    file.write_all(b" ").await?;
    file.write_all(&line_buf).await?;
    file.write_all(b"\n").await?;

    //Update the bytes written
    self.bytes_written += read;
    if let Some(max_log_size) = self.max_log_size() {
        if self.bytes_written > max_log_size {
            self.rotate().await?;
        }
    }

    //Write the partial line
    if let Some(partial) = partial {
        file.write_all(timestamp.as_bytes()).await?;
        file.write_all(b" ").await?;
        file.write_all(pipe.as_bytes()).await?;
        file.write_all(b" ").await?;
        file.write_all(partial).await?;
        file.write_all(b"\n").await?;
    }
    if let Some(max_log_size) = self.max_log_size() {
        if self.bytes_written > max_log_size {
            self.rotate().await?;
        }
    
    if new_bytes_written > max_log_size {
        new_bytes_written = 0;
        self.reopen()
            .await
            .context("reopen logs because of exceeded size")?;
    }
}

//Write the timestamp
let file = self.file.as_mut().context(Self::ERR_UNINITIALIZED)?;
file.write_all(timestamp.as_bytes()).await?;

match pipe {
    Pipe::StdOut => file.write_all(b" stdout ").await,
    Pipe::StdErr => file.write_all(b" stderr ").await,
}?;

//Output log tag for partial or newline
if partial {
    file.write_all(b"P ").await?;
} else {
    file.write_all(b"F ").await?;
}
//Output the actual contents
file.write_all(line_buf).await?;

//Output a newline for partial
if partial {
    file.write_all(b"\n").await?;
}

self.flush().await?;
}

//Rotate the log file
pub async fn rotate(&mut self) -> Result<()> {
    let log_path = self.path();
    let new_log_path = log_path.with_extension("1");
    debug!(
        "Rotating log file {} to {}",
        log_path.display(),
        new_log_path.display()
    );

    //Close the current file
    self.close().await?;

    //Rename the current file
    tokio::fs::rename(log_path, new_log_path)
        .await
        .context("rename log file")?;

    //Reopen the file
    self.init().await?;

    /// Ensures that all content is written to disk.
    pub async fn flush(&mut self) -> Result<()> {
        self.file
            .as_mut()
            .context(Self::ERR_UNINITIALIZED)?
            .flush()
            .await
            .context("flush file writer")
    }

    Ok(())
}
}
}
//Need help with opening the provided path above
#[cfg(test)]

mod tests {
    use super::*;
    use std::fs;
    use tempfile::NamedTempFile;
    use time::{format_description::FormatItem, OffsetDateTime};

    #[tokio::test]
    async fn write_stdout_success() -> Result<()> {
        let buffer = "test line to be converted to bytes";
        let bytes = buffer.as_bytes();
        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = JSONLogger::new(path, None)?;
        sut.init().await?;

        sut.write(Pipe::StdOut, bytes).await?;

        let res = fs::read_to_string(path)?;
        assert!(res.contains(" stdout F this is a line"));
        assert!(res.contains(" stdout F and another line"));

        let timestamp = res.split_whitespace().next().context("no timestamp")?;
        OffsetDateTime::parse(timestamp, &Rfc3339).context("unable to parse timestamp")?;
        Ok(())

    }

    #[tokio::test]
    async fn write_stdout_stderr_success() -> Result<()> {
        let buffer = "some random shit";
        let bytes = buffer.as_bytes();

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = JSONLogger::new(path, None)?;
        sut.init().await?;

        sut.write(Pipe::StdOut,bytes).await?;

        let res = fs::read_to_string(path)?;
        assert!(res.contains(" stdout F s"));
        assert!(res.contains(" stdout F r"));
        assert!(res.contains(" stdout F h"));

        Ok(())
    }

    #[tokio::test]
    async fn write_reopen() -> Result<()> {
        let buffer = "a\nb\nc\nd\ne\nf\n";
        let bytes = buffer.as_bytes();

        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = JSONLogger::new(path, Some(150))?;
        sut.init().await?;

        sut.write(Pipe::StdOut, bytes).await?;

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
    async fn write_multi_reopen() -> Result<()> {
        let file = NamedTempFile::new()?;
        let path = file.path();
        let mut sut = JSONLogger::new(path, Some(150))?;
        sut.init().await?;

        sut.write(Pipe::StdOut, "abcd\nabcd\nabcd\n".as_bytes())
            .await?;
        sut.write(Pipe::StdErr, "a\nb\nc\n".as_bytes()).await?;

        let res = fs::read_to_string(path)?;
        assert!(!res.contains(" stdout "));
        assert!(res.contains(" stderr F a"));
        assert!(res.contains(" stderr F b"));
        assert!(res.contains(" stderr F c"));
        Ok(())
    }

    #[tokio::test]
    async fn init_failure() -> Result<()> {
        let mut sut = JSONLogger::new("/file/doesn't/exist", None)?;
        assert!(sut.init().await.is_err());
        Ok(())
    } 
} 




    
