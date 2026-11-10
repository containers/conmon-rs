//! The log drivers of the containers, which write the container output to
//! the CRI log files or to journald.

mod cri;
mod journald;
mod line_buffer;
mod log_file;
#[cfg(test)]
pub(crate) mod testing;

use crate::conmon_capnp::conmon::log_driver::{self, Owned, Type};
use crate::{
    container_io::Pipe,
    container_log::{
        cri::CriLogger, journald::JournaldLogger, line_buffer::LineBuffer, log_file::LogFile,
    },
    fs_util::check_absolute,
    server::Server,
};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use capnp::struct_list::Reader;
use std::{io, ops::Deref, path::Path, sync::Arc};
use tokio::{
    sync::{Mutex, Semaphore},
    task,
};

/// A container log shared by its users, like the read loops of the stdio
/// and the log reopening.
pub(crate) type SharedContainerLog = Arc<Mutex<ContainerLog>>;

/// A configured log driver.
///
/// A driver either takes the raw chunks as they are read, like the CRI log
/// which writes partial lines right away, or whole lines which
/// [`ContainerLog`] collects across chunks once for all such drivers.
#[derive(Debug)]
enum Driver {
    Cri(CriLogger),
    Journald(JournaldLogger),
}

impl Driver {
    /// Whether the driver takes whole lines instead of raw chunks.
    fn buffers_lines(&self) -> bool {
        match self {
            Driver::Cri(_) => false,
            Driver::Journald(_) => true,
        }
    }

    /// The log file of the driver, none for one which writes elsewhere.
    fn file(&mut self) -> Option<&mut LogFile> {
        match self {
            Driver::Cri(logger) => Some(logger.file()),
            Driver::Journald(_) => None,
        }
    }

    /// Prepare the driver, open its file.
    async fn init(&mut self) -> Result<()> {
        match self.file() {
            Some(file) => file.open().await,
            None => Ok(()),
        }
    }

    /// Start over with a new file for log rotation.
    async fn reopen(&mut self) -> Result<()> {
        match self.file() {
            Some(file) => file.reopen().await,
            None => Ok(()),
        }
    }

    /// Write a raw chunk of output of the given pipe, if the driver takes
    /// chunks.
    async fn write_chunk(&mut self, pipe: Pipe, bytes: &[u8]) -> Result<()> {
        match self {
            Driver::Cri(logger) => logger.write_chunk(pipe, bytes).await,
            Driver::Journald(_) => Ok(()),
        }
    }

    /// Write whole lines of output of the given pipe, if the driver takes
    /// lines.
    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line<'_>]) -> Result<()> {
        match self {
            Driver::Cri(_) => Ok(()),
            Driver::Journald(logger) => logger.write_lines(pipe, lines).await,
        }
    }
}

/// The log drivers of a container.
#[derive(Debug, Default)]
pub(crate) struct ContainerLog {
    drivers: Vec<Driver>,

    /// Collects whole lines for the journald drivers, the only ones which
    /// take them.
    lines: LineBuffer,

    /// Whether any driver takes whole lines.
    buffers_lines: bool,
}

impl ContainerLog {
    /// Create a new default SharedContainerLog.
    pub(crate) fn new() -> SharedContainerLog {
        Arc::new(Mutex::new(Self::default()))
    }

    /// Check if this logger has any drivers configured.
    pub(crate) fn has_drivers(&self) -> bool {
        !self.drivers.is_empty()
    }

    /// Create a new SharedContainerLog for the container `id` from the
    /// requested drivers.
    pub(crate) fn from_request(id: &str, reader: Reader<Owned>) -> Result<SharedContainerLog> {
        let drivers = reader
            .iter()
            .map(|driver| -> Result<Driver> {
                let max_size = Self::max_size(&driver);
                Ok(match driver.get_type()? {
                    Type::ContainerRuntimeInterface => {
                        Driver::Cri(CriLogger::new(Self::path(&driver)?, max_size))
                    }
                    // Retired, `createContainer` rejects it with the other
                    // retired fields before, see `reject_retired`.
                    Type::JsonOld => {
                        debug_assert!(false, "the retired log driver type got through");
                        bail!("unsupported log driver type")
                    }
                    Type::Journald => Driver::Journald(JournaldLogger::new(
                        id,
                        driver.get_container_name()?.to_str()?,
                    )),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let buffers_lines = drivers.iter().any(Driver::buffers_lines);
        Ok(Arc::new(Mutex::new(Self {
            drivers,
            lines: LineBuffer::default(),
            buffers_lines,
        })))
    }

    /// The path of a requested driver, which has to be absolute.
    fn path<'a>(driver: &log_driver::Reader<'a>) -> Result<&'a Path> {
        let path = Path::new(driver.get_path()?.to_str()?);
        check_absolute("log path", path)?;
        Ok(path)
    }

    /// The maximum log size of a requested driver, zero means unlimited.
    fn max_size(driver: &log_driver::Reader<'_>) -> Option<usize> {
        match driver.get_max_size() {
            0 => None,
            // More than the address space is no limit either.
            max => usize::try_from(max).ok(),
        }
    }

    /// Asynchronously initialize all loggers.
    pub(crate) async fn init(&mut self) -> Result<()> {
        for driver in &mut self.drivers {
            driver.init().await?;
        }
        Ok(())
    }

    /// Reopen the container logs. All drivers get reopened even if one
    /// fails, the first error is returned.
    pub(crate) async fn reopen(&mut self) -> Result<()> {
        let mut result = Ok(());
        for driver in &mut self.drivers {
            result = result.and(driver.reopen().await);
        }
        result
    }

    /// Write the contents of the provided bytes into all loggers. All of
    /// them get the chunk even if one fails, the first error is returned.
    pub(crate) async fn write(&mut self, pipe: Pipe, bytes: &[u8]) -> Result<()> {
        let mut result = Ok(());
        for driver in &mut self.drivers {
            result = result.and(driver.write_chunk(pipe, bytes).await);
        }
        if self.buffers_lines {
            let lines = self.lines.push(pipe, bytes);
            let res = self.write_lines(pipe, &lines).await;
            result = result.and(res);
        }
        result
    }

    /// Whether a line of the pipe waits for its ending.
    pub(crate) fn has_pending(&self, pipe: Pipe) -> bool {
        self.buffers_lines && self.lines.has_pending(pipe)
    }

    /// Write the line of the pipe which waits for its ending as a partial
    /// one, because the output paused.
    pub(crate) async fn flush(&mut self, pipe: Pipe) -> Result<()> {
        let lines = self.lines.flush(pipe);
        self.write_lines(pipe, &lines).await
    }

    /// The output of the pipe ended, write the line which still waits for
    /// its ending.
    pub(crate) async fn finish(&mut self, pipe: Pipe) -> Result<()> {
        let lines = self.lines.finish(pipe);
        self.write_lines(pipe, &lines).await
    }

    /// Sync the data of the log files to disk, like conmon does once the
    /// container exited. The log is not locked while the files sync, they
    /// get synced even if one fails, the first error is returned. A file
    /// which cannot be synced, like a device, has nothing to sync. Only a
    /// few logs sync at a time, syncs which hang on a stuck disk keep a
    /// quarter of the threads of the blocking pool at most, see
    /// [`Server::MAX_BLOCKING_THREADS`]. Tests, which share the static, do
    /// not wait for each other.
    pub(crate) async fn sync(log: &SharedContainerLog) -> Result<()> {
        const PERMITS: usize = if cfg!(test) {
            Semaphore::MAX_PERMITS
        } else {
            Server::MAX_BLOCKING_THREADS / 4
        };
        static SYNC: Semaphore = Semaphore::const_new(PERMITS);
        let files: Vec<_> = log
            .lock()
            .await
            .drivers
            .iter_mut()
            .filter_map(|driver| driver.file()?.file.clone())
            .collect();
        if files.is_empty() {
            return Ok(());
        }
        let permit = SYNC.acquire().await.context("wait for log sync")?;
        task::spawn_blocking(move || {
            let _permit = permit;
            files
                .iter()
                .map(|file| match file.sync_data() {
                    Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(()),
                    res => res,
                })
                .fold(Ok(()), io::Result::and)
        })
        .await
        .context("join log sync")?
        .context("sync log file")
    }

    /// Hand the lines to every driver which takes them. All of them get the
    /// lines even if one fails, the first error is returned.
    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line<'_>]) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut result = Ok(());
        for driver in self.drivers.iter_mut().filter(|d| d.buffers_lines()) {
            result = result.and(driver.write_lines(pipe, lines).await);
        }
        result
    }
}

/// Split a chunk into its lines. Every line but the last ends with a newline,
/// the last one is partial if the chunk does not end with one. An empty chunk
/// has no lines.
pub(crate) fn lines(bytes: &[u8]) -> impl Iterator<Item = (&[u8], bool)> {
    let mut ends = memchr::memchr_iter(b'\n', bytes);
    let mut start = 0;
    std::iter::from_fn(move || {
        let (end, partial) = match ends.next() {
            Some(end) => (end + 1, false),
            None if start < bytes.len() => (bytes.len(), true),
            None => return None,
        };
        let line = &bytes[start..end];
        start = end;
        Some((line, partial))
    })
}

/// Strip the line ending, which a terminal turns into "\r\n".
fn strip_line_ending(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r\n")
        .or_else(|| line.strip_suffix(b"\n"))
        .unwrap_or(line)
}

/// A whole or partial line without its ending. Partial means that the line
/// had no ending yet: it continues in the next entry, paused, or the output
/// ended without one, the same as the `P` marker of the CRI log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Line<'a> {
    pub(crate) text: Text<'a>,
    pub(crate) partial: bool,
}

impl Line<'_> {
    /// The line with a text of its own, for a task which outlives the chunk.
    pub(crate) fn into_owned(self) -> Line<'static> {
        Line {
            text: self.text.into_owned(),
            partial: self.partial,
        }
    }
}

/// The text of a [`Line`]: borrowed from the chunk which held all of it, or
/// collected across chunks in the buffer of its pipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Text<'a> {
    Chunk(&'a [u8]),
    Buffered(Bytes),
}

impl Text<'_> {
    fn into_owned(self) -> Text<'static> {
        match self {
            Text::Chunk(text) => Text::Buffered(Bytes::copy_from_slice(text)),
            Text::Buffered(text) => Text::Buffered(text),
        }
    }
}

impl Deref for Text<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Text::Chunk(text) => text,
            Text::Buffered(text) => text,
        }
    }
}

/// The entry points of the fuzz targets, see `crate::fuzzing`.
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) mod fuzzing;

#[cfg(test)]
mod tests;
