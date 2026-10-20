mod cri;
mod journald;
mod json;

use crate::{
    container_io::Pipe, container_log::cri::CriLogger, container_log::journald::JournaldLogger,
    container_log::json::JsonLogger,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use capnp::struct_list::Reader;
use conmon_common::conmon_capnp::conmon::log_driver::{Owned, Type};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
    sync::RwLock,
};
use tracing::{debug, trace};

pub type SharedContainerLog = Arc<RwLock<ContainerLog>>;

#[derive(Debug, Default)]
pub struct ContainerLog {
    drivers: Vec<LogDriver>,
}

#[derive(Debug)]
enum LogDriver {
    ContainerRuntimeInterface(CriLogger),
    Journald(JournaldLogger),
    Json(JsonLogger),
}

impl ContainerLog {
    /// Create a new default SharedContainerLog.
    pub fn new() -> SharedContainerLog {
        Arc::new(RwLock::new(Self::default()))
    }

    /// Check if this logger has any drivers configured.
    pub fn has_drivers(&self) -> bool {
        !self.drivers.is_empty()
    }

    /// Create a new SharedContainerLog from an owned reader.
    pub fn from(reader: Reader<Owned>) -> Result<SharedContainerLog> {
        let drivers = reader
            .iter()
            .map(|x| -> Result<_> {
                match x.get_type()? {
                    Type::ContainerRuntimeInterface => {
                        Ok(LogDriver::ContainerRuntimeInterface(CriLogger::new(
                            x.get_path()?.to_str()?,
                            if x.get_max_size() > 0 {
                                Some(x.get_max_size() as usize)
                            } else {
                                None
                            },
                        )?))
                    }
                    Type::Json => Ok(LogDriver::Json(JsonLogger::new(
                        x.get_path()?.to_str()?,
                        if x.get_max_size() > 0 {
                            Some(x.get_max_size() as usize)
                        } else {
                            None
                        },
                    )?)),
                    Type::Journald => Ok(LogDriver::Journald(JournaldLogger::new(
                        if x.get_max_size() > 0 {
                            Some(x.get_max_size() as usize)
                        } else {
                            None
                        },
                    )?)),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(RwLock::new(Self { drivers })))
    }

    /// Asynchronously initialize all loggers.
    pub async fn init(&mut self) -> Result<()> {
        for driver in &mut self.drivers {
            match driver {
                LogDriver::ContainerRuntimeInterface(logger) => logger.init().await?,
                LogDriver::Json(logger) => logger.init().await?,
                LogDriver::Journald(logger) => logger.init().await?,
            }
        }
        Ok(())
    }

    /// Reopen the container logs.
    pub async fn reopen(&mut self) -> Result<()> {
        for driver in &mut self.drivers {
            match driver {
                LogDriver::ContainerRuntimeInterface(logger) => logger.reopen().await?,
                LogDriver::Json(logger) => logger.reopen().await?,
                LogDriver::Journald(logger) => logger.reopen().await?,
            }
        }
        Ok(())
    }

    /// Write the contents of the provided bytes into all loggers.
    pub async fn write(&mut self, pipe: Pipe, bytes: &Bytes) -> Result<()> {
        for driver in &mut self.drivers {
            match driver {
                LogDriver::ContainerRuntimeInterface(logger) => logger.write(pipe, bytes).await?,
                LogDriver::Journald(logger) => logger.write(pipe, bytes.clone()).await?,
                LogDriver::Json(logger) => logger.write(pipe, bytes).await?,
            }
        }
        Ok(())
    }
}

/// A log file on disk which is written in whole chunks.
///
/// The drivers assemble their entries into `buf`, account each one with
/// [`LogFile::commit_entry`] and write the buffer with a single call per read
/// chunk. The file is an async one on purpose: the write and the truncating
/// open run on the blocking pool, so writeback throttling, a slow disk or
/// truncating a huge log only stalls this log and not the thread which drives
/// every container of the pod.
#[derive(Debug)]
pub(crate) struct LogFile {
    path: PathBuf,
    mode: Option<u32>,
    file: Option<File>,

    /// Maximum allowed log size in bytes.
    max_size: Option<usize>,

    /// Bytes written to the file since it got opened, including the
    /// committed entries of `buf`.
    bytes_written: usize,

    /// The pending output of the current chunk.
    pub(crate) buf: Vec<u8>,
}

impl LogFile {
    const ERR_UNINITIALIZED: &'static str = "logger not initialized";

    /// Initial capacity of the chunk buffer, a read chunk plus line overhead.
    const BUF_CAPACITY: usize = 8 * 1024;

    /// Buffers which grew beyond this size get shrunk again after a write.
    const BUF_RETAIN_LIMIT: usize = 64 * 1024;

    pub(crate) fn new(path: impl AsRef<Path>, mode: Option<u32>, max_size: Option<usize>) -> Self {
        Self {
            path: path.as_ref().into(),
            mode,
            file: None,
            max_size,
            bytes_written: 0,
            buf: Vec::with_capacity(Self::BUF_CAPACITY),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Open the file, truncating any existing content.
    pub(crate) async fn open(&mut self) -> Result<()> {
        let mut options = OpenOptions::new();
        options.create(true).read(true).truncate(true).write(true);
        if let Some(mode) = self.mode {
            options.mode(mode);
        }
        let file = options
            .open(&self.path)
            .await
            .with_context(|| format!("open log file path '{}'", self.path.display()))?;
        self.file = Some(file);
        self.bytes_written = 0;
        Ok(())
    }

    /// Account for the entry appended to `buf` since `start`. If it would
    /// exceed the maximum log size, the file gets truncated and the entry
    /// becomes the first one of the new file. The entries of the chunk before
    /// it are dropped instead of written, the truncation would discard them
    /// right away.
    pub(crate) async fn commit_entry(&mut self, start: usize) -> Result<()> {
        let len = self.buf.len() - start;
        let total = self.bytes_written.saturating_add(len);
        trace!(
            "Verifying log size: max_size = {:?}, bytes_written = {}, entry = {}",
            self.max_size, self.bytes_written, len,
        );

        if self.max_size.is_some_and(|max| total > max) {
            debug!("Rotate log file {}", self.path.display());
            if let Err(e) = self.open().await {
                // The old file stays in place. Drop the uncounted entry and
                // write the ones which fit into it now, the caller stops on the
                // error and may not write again.
                self.buf.truncate(start);
                if let Err(e) = self.flush().await {
                    debug!("Unable to flush log file after failed rotation: {:#}", e);
                }
                return Err(e).context("reopen log file because of exceeded size");
            }
            self.buf.drain(..start);
            self.bytes_written = len;
        } else {
            self.bytes_written = total;
        }
        Ok(())
    }

    /// Write the pending buffer to the file and clear it.
    pub(crate) async fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let res = self.write_buf().await;
        if res.is_err() {
            // Drop the chunk even if the write failed. It may have been written
            // partially, retrying it along with the next chunk would duplicate
            // that part. Assume it did not make it, so that a later rotation
            // does not happen early.
            self.bytes_written = self.bytes_written.saturating_sub(self.buf.len());
        }
        self.buf.clear();
        if self.buf.capacity() > Self::BUF_RETAIN_LIMIT {
            self.buf.shrink_to(Self::BUF_CAPACITY);
        }
        res
    }

    async fn write_buf(&mut self) -> Result<()> {
        let file = self.file.as_mut().context(Self::ERR_UNINITIALIZED)?;
        file.write_all(&self.buf).await.context("write log chunk")?;
        file.flush().await.context("flush log file")
    }

    /// Write the pending buffer and reopen the file, which truncates it.
    ///
    /// The file is not synced to disk: an fsync of a large log can take long
    /// and the data sits in the page cache either way.
    pub(crate) async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen log file {}", self.path.display());
        self.flush().await.context("flush log file before reopen")?;
        self.open().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn reopen_resets_size() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut sut = LogFile::new(file.path(), None, Some(10));
        sut.open().await?;

        sut.buf.extend_from_slice(b"12345678\n");
        sut.commit_entry(0).await?;
        sut.flush().await?;
        sut.reopen().await?;

        // The reopened file is empty, so the entry fits without a rotation.
        sut.buf.extend_from_slice(b"abc\n");
        sut.commit_entry(0).await?;
        sut.buf.extend_from_slice(b"def\n");
        sut.commit_entry(4).await?;
        sut.flush().await?;

        assert_eq!(fs::read_to_string(file.path())?, "abc\ndef\n");
        assert_eq!(sut.bytes_written, 8);
        Ok(())
    }

    #[tokio::test]
    async fn rotation_drops_earlier_entries_of_chunk() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut sut = LogFile::new(file.path(), None, Some(10));
        sut.open().await?;

        sut.buf.extend_from_slice(b"first\n");
        sut.commit_entry(0).await?;
        sut.buf.extend_from_slice(b"second\n");
        sut.commit_entry(6).await?;
        assert_eq!(sut.buf, b"second\n");
        sut.flush().await?;

        assert_eq!(fs::read_to_string(file.path())?, "second\n");
        assert_eq!(sut.bytes_written, 7);
        Ok(())
    }

    #[tokio::test]
    async fn failed_rotation_writes_old_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("log");
        let mut sut = LogFile::new(&path, None, Some(10));
        sut.open().await?;

        // Keep the open file reachable, then make the truncating open fail by
        // putting a directory in its place.
        let old = dir.path().join("old");
        fs::hard_link(&path, &old)?;
        fs::remove_file(&path)?;
        fs::create_dir(&path)?;

        sut.buf.extend_from_slice(b"first\n");
        sut.commit_entry(0).await?;
        sut.buf.extend_from_slice(b"second\n");
        assert!(sut.commit_entry(6).await.is_err());

        assert!(sut.buf.is_empty());
        assert_eq!(sut.bytes_written, 6);
        assert_eq!(fs::read_to_string(&old)?, "first\n");
        Ok(())
    }

    #[tokio::test]
    async fn failed_flush_drops_chunk() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut sut = LogFile::new(file.path(), None, None);

        // Not opened yet, so the write fails.
        sut.buf.extend_from_slice(b"lost\n");
        sut.commit_entry(0).await?;
        assert!(sut.flush().await.is_err());
        assert!(sut.buf.is_empty());
        assert_eq!(sut.bytes_written, 0);

        sut.open().await?;
        sut.buf.extend_from_slice(b"kept\n");
        sut.flush().await?;
        assert_eq!(fs::read_to_string(file.path())?, "kept\n");
        Ok(())
    }
}
