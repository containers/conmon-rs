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
use tracing::debug;

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
/// The drivers assemble their lines into `buf` and the buffer is written with
/// a single call per read chunk. The file is an async one on purpose: the
/// write and the truncating open run on the blocking pool, so writeback
/// throttling, a slow disk or truncating a huge log only stalls this log and
/// not the thread which drives every container of the pod.
#[derive(Debug)]
pub(crate) struct LogFile {
    path: PathBuf,
    mode: Option<u32>,
    file: Option<File>,

    /// The pending output of the current chunk.
    pub(crate) buf: Vec<u8>,
}

impl LogFile {
    const ERR_UNINITIALIZED: &'static str = "logger not initialized";

    /// Initial capacity of the chunk buffer, a read chunk plus line overhead.
    const BUF_CAPACITY: usize = 8 * 1024;

    /// Buffers which grew beyond this size get shrunk again after a write.
    const BUF_RETAIN_LIMIT: usize = 64 * 1024;

    pub(crate) fn new(path: impl AsRef<Path>, mode: Option<u32>) -> Self {
        Self {
            path: path.as_ref().into(),
            mode,
            file: None,
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
        Ok(())
    }

    /// Write the pending buffer to the file and clear it.
    pub(crate) async fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let file = self.file.as_mut().context(Self::ERR_UNINITIALIZED)?;
        file.write_all(&self.buf).await.context("write log chunk")?;
        file.flush().await.context("flush log file")?;
        self.buf.clear();
        if self.buf.capacity() > Self::BUF_RETAIN_LIMIT {
            self.buf.shrink_to(Self::BUF_CAPACITY);
        }
        Ok(())
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
