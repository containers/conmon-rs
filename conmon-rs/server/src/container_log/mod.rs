mod cri;
mod journald;
mod json;
#[cfg(test)]
pub(crate) mod test_helpers;

use crate::conmon_capnp::conmon::log_driver::{self, Owned, Type};
use crate::{
    capnp_util::{check_absolute, into_map},
    container_io::Pipe,
    container_log::cri::CriLogger,
    container_log::journald::JournaldLogger,
    container_log::json::JsonLogger,
};
use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use capnp::struct_list::Reader;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    mem,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{sync::RwLock, task};
use tracing::{debug, error, trace};

pub type SharedContainerLog = Arc<RwLock<ContainerLog>>;

/// A destination for the output of a container.
///
/// A driver either takes the raw chunks as they are read, like the CRI log
/// which writes partial lines right away, or whole lines which
/// [`ContainerLog`] collects across chunks once for all such drivers.
trait Logger {
    /// Whether the driver takes whole lines instead of raw chunks.
    const BUFFERS_LINES: bool;

    /// Prepare the logger, for example open its file.
    async fn init(&mut self) -> Result<()>;

    /// Start over, for example with a new file for log rotation.
    async fn reopen(&mut self) -> Result<()>;

    /// Write a raw chunk of output of the given pipe.
    async fn write_chunk(&mut self, _: Pipe, _: &[u8]) -> Result<()> {
        Ok(())
    }

    /// Write whole lines of output of the given pipe.
    async fn write_lines(&mut self, _: Pipe, _: &[Line]) -> Result<()> {
        Ok(())
    }
}

/// The configured log drivers. An enum rather than trait objects, because
/// async trait methods are not dyn compatible without boxing every call.
#[derive(Debug)]
enum Driver {
    Cri(CriLogger),
    Json(JsonLogger),
    Journald(JournaldLogger),
}

/// Call a [`Logger`] method on whichever driver this is.
macro_rules! dispatch {
    ($self:expr, $logger:ident => $call:expr) => {
        match $self {
            Driver::Cri($logger) => $call,
            Driver::Json($logger) => $call,
            Driver::Journald($logger) => $call,
        }
    };
}

impl Driver {
    fn buffers_lines(&self) -> bool {
        match self {
            Driver::Cri(_) => CriLogger::BUFFERS_LINES,
            Driver::Json(_) => JsonLogger::BUFFERS_LINES,
            Driver::Journald(_) => JournaldLogger::BUFFERS_LINES,
        }
    }

    async fn init(&mut self) -> Result<()> {
        dispatch!(self, logger => logger.init().await)
    }

    async fn reopen(&mut self) -> Result<()> {
        dispatch!(self, logger => logger.reopen().await)
    }

    async fn write_chunk(&mut self, pipe: Pipe, bytes: &[u8]) -> Result<()> {
        dispatch!(self, logger => logger.write_chunk(pipe, bytes).await)
    }

    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line]) -> Result<()> {
        dispatch!(self, logger => logger.write_lines(pipe, lines).await)
    }
}

#[derive(Debug, Default)]
pub struct ContainerLog {
    drivers: Vec<Driver>,

    /// Collects whole lines for the drivers which take them, shared by all
    /// of them.
    lines: LineBuffer,

    /// Whether any driver takes whole lines.
    buffers_lines: bool,
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

    /// Create a new SharedContainerLog for the container `id` from the
    /// requested drivers.
    pub fn from_request(id: &str, reader: Reader<Owned>) -> Result<SharedContainerLog> {
        let drivers = reader
            .iter()
            .map(|driver| -> Result<Driver> {
                let max_size = Self::max_size(&driver);
                Ok(match driver.get_type()? {
                    Type::ContainerRuntimeInterface => {
                        Driver::Cri(CriLogger::new(Self::path(&driver)?, max_size))
                    }
                    Type::Json => Driver::Json(JsonLogger::new(Self::path(&driver)?, max_size)),
                    Type::Journald => {
                        let labels: Vec<(&str, &str)> = into_map(driver.get_labels()?)?;
                        Driver::Journald(JournaldLogger::new(
                            id,
                            driver.get_container_name()?.to_str()?,
                            driver.get_tag()?.to_str()?,
                            labels,
                        )?)
                    }
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let buffers_lines = drivers.iter().any(Driver::buffers_lines);
        Ok(Arc::new(RwLock::new(Self {
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
    pub async fn init(&mut self) -> Result<()> {
        for driver in &mut self.drivers {
            driver.init().await?;
        }
        Ok(())
    }

    /// Reopen the container logs. All drivers get reopened even if one
    /// fails, the first error is returned.
    pub async fn reopen(&mut self) -> Result<()> {
        let mut result = Ok(());
        for driver in &mut self.drivers {
            if let Err(e) = driver.reopen().await
                && result.is_ok()
            {
                result = Err(e);
            }
        }
        result
    }

    /// Write the contents of the provided bytes into all loggers. All of
    /// them get the chunk even if one fails, the first error is returned.
    pub async fn write(&mut self, pipe: Pipe, bytes: &[u8]) -> Result<()> {
        let mut result = Ok(());
        for driver in &mut self.drivers {
            if let Err(e) = driver.write_chunk(pipe, bytes).await
                && result.is_ok()
            {
                result = Err(e);
            }
        }
        if self.buffers_lines {
            let lines = self.lines.push(pipe, bytes);
            let res = self.write_lines(pipe, &lines).await;
            result = result.and(res);
        }
        result
    }

    /// Whether a line of the pipe waits for its ending.
    pub fn has_pending(&self, pipe: Pipe) -> bool {
        self.buffers_lines && self.lines.has_pending(pipe)
    }

    /// Write the line of the pipe which waits for its ending as a partial
    /// one, because the output paused.
    pub async fn flush(&mut self, pipe: Pipe) -> Result<()> {
        let lines = self.lines.flush(pipe);
        self.write_lines(pipe, &lines).await
    }

    /// The output of the pipe ended, write the line which still waits for
    /// its ending.
    pub async fn finish(&mut self, pipe: Pipe) -> Result<()> {
        let lines = self.lines.finish(pipe);
        self.write_lines(pipe, &lines).await
    }

    /// Hand the lines to every driver which takes them. All of them get the
    /// lines even if one fails, the first error is returned.
    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line]) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut result = Ok(());
        for driver in &mut self.drivers {
            if driver.buffers_lines()
                && let Err(e) = driver.write_lines(pipe, lines).await
                && result.is_ok()
            {
                result = Err(e);
            }
        }
        result
    }
}

/// Split a chunk into its lines. Every line but the last ends with a newline,
/// the last one is partial if the chunk does not end with one.
pub(crate) fn lines(bytes: &[u8]) -> impl Iterator<Item = (&[u8], bool)> {
    bytes
        .split_inclusive(|&b| b == b'\n')
        .map(|line| (line, !line.ends_with(b"\n")))
}

/// Strip the line ending, which a terminal turns into "\r\n".
pub(crate) fn strip_line_ending(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r\n")
        .or_else(|| line.strip_suffix(b"\n"))
        .unwrap_or(line)
}

/// A whole or partial line without its ending. Partial means that the line
/// had no ending yet: it continues in the next entry, paused, or the output
/// ended without one, the same as the `P` marker of the CRI log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) text: Bytes,
    pub(crate) partial: bool,
}

/// Collects the output of both pipes into whole lines for the drivers which
/// write one entry per line.
///
/// A read chunk may end anywhere, in the middle of a line, between "\r" and
/// "\n" or inside a multi byte character. Keeping the incomplete tail until
/// the next chunk of the same pipe arrives avoids split lines and garbled
/// characters. A line which grows beyond [`LineBuffer::MAX_LINE`] is written
/// as a partial one, cut at a character boundary, and so is a line which
/// waits too long for its ending, see [`LineBuffer::flush`].
#[derive(Debug, Default)]
pub(crate) struct LineBuffer {
    stdout: BytesMut,
    stderr: BytesMut,
}

impl LineBuffer {
    /// Longer lines get written as partial entries.
    const MAX_LINE: usize = 16 * 1024;

    fn tail(&mut self, pipe: Pipe) -> &mut BytesMut {
        match pipe {
            Pipe::StdOut => &mut self.stdout,
            Pipe::StdErr => &mut self.stderr,
        }
    }

    /// Whether a line of the pipe waits for its ending.
    pub(crate) fn has_pending(&self, pipe: Pipe) -> bool {
        !match pipe {
            Pipe::StdOut => &self.stdout,
            Pipe::StdErr => &self.stderr,
        }
        .is_empty()
    }

    /// Add a chunk and return the lines it completes. The lines share the
    /// buffer, no copy is made.
    pub(crate) fn push(&mut self, pipe: Pipe, bytes: &[u8]) -> Vec<Line> {
        let tail = self.tail(pipe);
        tail.extend_from_slice(bytes);

        let mut lines = vec![];
        while let Some(end) = memchr::memchr(b'\n', tail) {
            let line = tail.split_to(end + 1).freeze();
            lines.push(Line {
                text: line.slice(..strip_line_ending(&line).len()),
                partial: false,
            });
        }

        if tail.len() >= Self::MAX_LINE {
            lines.push(Self::cut(tail));
        }
        Self::shrink(tail);
        lines
    }

    /// Cut the tail at a point which keeps an unfinished multi byte character
    /// and a "\r" waiting for its "\n" in the buffer. Invalid bytes are not
    /// held back, they would stay in front forever.
    fn cut(tail: &mut BytesMut) -> Line {
        let mut end = Self::complete_len(tail);
        if tail[..end].ends_with(b"\r") {
            end -= 1;
        }
        Line {
            text: tail.split_to(end).freeze(),
            partial: true,
        }
    }

    /// The length of the bytes without the unfinished multi byte character
    /// they end with, if any. Only the last character matters, which starts
    /// within the last three bytes if it is unfinished, an invalid byte in
    /// front of it does not.
    fn complete_len(bytes: &[u8]) -> usize {
        let window = bytes.len().saturating_sub(3);
        let Some(start) = bytes[window..]
            .iter()
            .rposition(|b| b & 0xc0 != 0x80)
            .map(|i| window + i)
        else {
            return bytes.len();
        };
        match str::from_utf8(&bytes[start..]) {
            Err(e) if e.error_len().is_none() => start + e.valid_up_to(),
            _ => bytes.len(),
        }
    }

    /// A long line leaves a large buffer behind, release it once drained.
    fn shrink(tail: &mut BytesMut) {
        if tail.is_empty() && tail.capacity() > Self::MAX_LINE {
            *tail = BytesMut::new();
        }
    }

    /// Return the line which waits for its ending as a partial one. The
    /// output paused, a prompt or progress output without a newline would
    /// otherwise stay invisible.
    pub(crate) fn flush(&mut self, pipe: Pipe) -> Vec<Line> {
        // Like a long line, it keeps an unfinished multi byte character and
        // a "\r" back for the next chunk, see [`LineBuffer::cut`].
        let tail = self.tail(pipe);
        if tail.is_empty() {
            return vec![];
        }
        let line = Self::cut(tail);
        Self::shrink(tail);
        if line.text.is_empty() {
            return vec![];
        }
        vec![line]
    }

    /// The output of the pipe ended, return the line which still waits for
    /// its ending as a partial one.
    pub(crate) fn finish(&mut self, pipe: Pipe) -> Vec<Line> {
        let tail = self.tail(pipe);
        if tail.is_empty() {
            return vec![];
        }
        // Without a newline, so there is no line ending to strip.
        let text = tail.split().freeze();
        Self::shrink(tail);
        vec![Line {
            text,
            partial: true,
        }]
    }
}

/// A log file on disk which is written in whole chunks.
///
/// The drivers assemble their entries into `buf`, account each one with
/// [`LogFile::commit_entry`] and write the buffer with a single call per read
/// chunk, or per [`LogFile::FLUSH_SIZE`] of entries for chunks of short lines.
/// The writes and the opens run on the blocking pool, so writeback
/// throttling, a slow disk or truncating a huge log only stalls this log and
/// not the thread which drives every container of the pod and the RPCs. The
/// buffer moves to the blocking task and back, unlike an async file, which
/// copies every write into a buffer of its own that keeps the size of the
/// largest one.
#[derive(Debug)]
pub(crate) struct LogFile {
    path: PathBuf,
    mode: Option<u32>,
    file: Option<Arc<File>>,

    /// Maximum allowed log size in bytes.
    max_size: Option<usize>,

    /// Bytes written to the file since it got opened, including the
    /// committed entries of `buf`.
    bytes_written: usize,

    /// The pending output of the current chunk, allocated on first use, see
    /// [`LogFile::buf`].
    buf: Vec<u8>,

    /// Whether a failed write left the file in the middle of an entry, so
    /// that the next one has to start on a line of its own.
    torn: bool,
}

impl LogFile {
    const ERR_UNINITIALIZED: &'static str = "logger not initialized";

    /// Initial capacity of the chunk buffer, it grows to what the read chunks
    /// need with their line overhead.
    const BUF_CAPACITY: usize = 8 * 1024;

    /// The entries get written once they take this much, a chunk of very
    /// short lines would grow the buffer to many times its size otherwise.
    const FLUSH_SIZE: usize = 16 * 1024;

    /// Buffers which grew beyond this size get shrunk again after a write,
    /// for example by a long line. The entry which crosses
    /// [`LogFile::FLUSH_SIZE`] grows the buffer up to twice that size, which
    /// is kept, shrinking and growing it again for every write would only
    /// churn the allocator.
    const BUF_RETAIN_LIMIT: usize = 2 * Self::FLUSH_SIZE;

    pub(crate) fn new(path: impl AsRef<Path>, mode: Option<u32>, max_size: Option<usize>) -> Self {
        Self {
            path: path.as_ref().into(),
            mode,
            file: None,
            max_size,
            bytes_written: 0,
            buf: Vec::new(),
            torn: false,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The buffer to append the entries of a chunk to. A container which
    /// never writes output does not need one.
    pub(crate) fn buf(&mut self) -> &mut Vec<u8> {
        if self.buf.capacity() == 0 {
            self.buf.reserve_exact(Self::BUF_CAPACITY);
        }
        &mut self.buf
    }

    /// Open the file, truncating any existing content. A symlink in place of
    /// the log is not followed, the truncation would hit its target. A
    /// temporary file which a crashed server left behind gets removed, the
    /// kubelet would never clean it up.
    pub(crate) async fn open(&mut self) -> Result<()> {
        let options = self.open_options();
        let path = self.path.clone();
        let file = task::spawn_blocking(move || {
            let tmp = Self::tmp_path(&path);
            if let Err(e) = fs::remove_file(&tmp)
                && e.kind() != io::ErrorKind::NotFound
            {
                debug!("Unable to remove {}: {e}", tmp.display());
            }
            options.open(path)
        })
        .await
        .context("join log file open")?
        .with_context(|| format!("open log file path '{}'", self.path.display()))?;
        self.file = Some(Arc::new(file));
        self.bytes_written = 0;
        self.torn = false;
        Ok(())
    }

    /// The new file of a replacement before it gets renamed over the path:
    /// `.<name>.conmonrs-tmp` next to it. The kubelet removes `*.tmp` files
    /// and treats `<name>.*` ones as rotated logs, a name which matches
    /// neither keeps it from deleting the file while it gets renamed.
    fn tmp_path(path: &Path) -> PathBuf {
        let mut name = std::ffi::OsString::from(".");
        name.push(path.file_name().unwrap_or_default());
        name.push(".conmonrs-tmp");
        path.with_file_name(name)
    }

    /// Replace the file with a new empty one, like conmon does: create a
    /// temporary file, see [`LogFile::tmp_path`], and rename it over the
    /// path. Unlike a truncation in place, a reader which follows the log,
    /// like `kubectl logs -f`, keeps the old file with its offset and reads
    /// it to its end, before it notices the new one. A rename does not follow
    /// a symlink at the path either.
    async fn replace(&mut self) -> Result<()> {
        let options = self.open_options();
        let path = self.path.clone();
        let tmp = Self::tmp_path(&path);
        let file = task::spawn_blocking(move || -> Result<File> {
            let file = options
                .open(&tmp)
                .with_context(|| format!("open log file path '{}'", tmp.display()))?;
            if let Err(e) = fs::rename(&tmp, &path) {
                if let Err(e) = fs::remove_file(&tmp) {
                    debug!("Unable to remove {}: {e}", tmp.display());
                }
                return Err(e).with_context(|| format!("rename log file to '{}'", path.display()));
            }
            Ok(file)
        })
        .await
        .context("join log file replace")??;
        self.file = Some(Arc::new(file));
        self.bytes_written = 0;
        self.torn = false;
        Ok(())
    }

    /// The writes append, so that they land at the end also after the file
    /// got truncated in place, like by an admin freeing disk space, instead
    /// of at the old offset, which leaves a hole of zeros in front of them.
    /// The standard library refuses append together with truncate, the
    /// kernel takes both.
    fn open_options(&self) -> OpenOptions {
        let mut options = OpenOptions::new();
        options
            .create(true)
            .read(true)
            .truncate(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_APPEND);
        if let Some(mode) = self.mode {
            options.mode(mode);
        }
        options
    }

    /// Account for the entry appended to `buf` since `start`. If it would
    /// exceed the maximum log size, the entries of the chunk before it go to
    /// the old file, which readers following it still read to its end, and
    /// the file gets replaced, the entry becomes the first one of the new
    /// file.
    pub(crate) async fn commit_entry(&mut self, start: usize) -> Result<()> {
        let len = self.buf.len() - start;
        let mut total = self.bytes_written.saturating_add(len);
        trace!(
            "Verifying log size: max_size = {:?}, bytes_written = {}, entry = {}",
            self.max_size, self.bytes_written, len,
        );

        // A file which got truncated in place, like by an admin freeing disk
        // space, holds less than counted, the writes continue at its new end.
        // Only rotate once the file itself grew beyond the size.
        if self.max_size.is_some_and(|max| total > max)
            && let Some(size) = self.file_size()
            && size.saturating_add(start) < self.bytes_written
        {
            debug!(
                "Log file {} shrank, counting from its size",
                self.path.display()
            );
            self.bytes_written = size.saturating_add(start);
            total = self.bytes_written.saturating_add(len);
        }

        if self.max_size.is_some_and(|max| total > max) {
            debug!("Rotate log file {}", self.path.display());
            let entry = self.buf.split_off(start);
            if let Err(e) = self.flush().await {
                error!("Unable to flush log file before rotation: {e:#}");
            }
            if let Err(e) = self.replace().await {
                // The old file stays in place. Drop the uncounted entry, the
                // next entry tries the rotation again, until then entries
                // which exceed the size get dropped.
                return Err(e).context("reopen log file because of exceeded size");
            }
            self.buf.extend_from_slice(&entry);
            self.bytes_written = len;
        } else {
            self.bytes_written = total;
        }
        if self.buf.len() >= Self::FLUSH_SIZE {
            self.flush().await?;
        }
        Ok(())
    }

    /// The size of the open file. A stat of an open file does not wait for
    /// the disk, unlike its writes.
    fn file_size(&self) -> Option<usize> {
        let metadata = self.file.as_ref()?.metadata().ok()?;
        usize::try_from(metadata.len()).ok()
    }

    /// Write the pending buffer to the file and clear it.
    pub(crate) async fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let len = self.buf.len();
        let (written, res) = self.write_buf().await;
        if res.is_err() {
            // Drop the chunk even if the write failed, retrying it along with
            // the next chunk would duplicate the part which got written. Only
            // that part counts for the rotation.
            self.bytes_written = self.bytes_written.saturating_sub(len - written);
        }
        self.buf.clear();
        if self.buf.capacity() > Self::BUF_RETAIN_LIMIT {
            self.buf.shrink_to(Self::BUF_CAPACITY);
        }
        res
    }

    /// Write the buffer and return how much of it got written. A write which
    /// fails in the middle of an entry, like on a full disk, leaves the file
    /// torn, the next write then starts with a newline of its own, so that
    /// the next entry does not get appended to the cut one.
    async fn write_buf(&mut self) -> (usize, Result<()>) {
        let Some(file) = self.file.clone() else {
            return (0, Err(anyhow::anyhow!(Self::ERR_UNINITIALIZED)));
        };
        let buf = mem::take(&mut self.buf);
        let torn = self.torn;
        let (buf, newline, written, res) = match task::spawn_blocking(move || {
            let (newline, written, res) = Self::write_out(&file, torn, &buf);
            (buf, newline, written, res)
        })
        .await
        {
            Ok(res) => res,
            // The buffer went with the task, the next entry allocates one.
            Err(e) => return (0, Err(e).context("join log write")),
        };
        self.buf = buf;
        if newline {
            self.bytes_written = self.bytes_written.saturating_add(1);
        }
        self.torn = if written > 0 {
            res.is_err() && !self.buf[..written].ends_with(b"\n")
        } else {
            torn && !newline
        };
        (written, res.context("write log chunk"))
    }

    /// Write `buf` to the file, after a newline if the file is `torn`.
    /// Return whether the newline got written and how much of the buffer.
    fn write_out(mut file: &File, torn: bool, buf: &[u8]) -> (bool, usize, io::Result<()>) {
        if torn && let Err(e) = file.write_all(b"\n") {
            return (false, 0, Err(e));
        }
        let mut written = 0;
        while written < buf.len() {
            match file.write(&buf[written..]) {
                Ok(0) => return (torn, written, Err(io::ErrorKind::WriteZero.into())),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return (torn, written, Err(e)),
            }
        }
        (torn, written, Ok(()))
    }

    /// Write the pending buffer and start a new file at the path. The
    /// kubelet renamed the old one already, a file still at the path gets
    /// replaced, like by the rotation.
    ///
    /// The file is not synced to disk: an fsync of a large log can take long
    /// and the data sits in the page cache either way.
    ///
    /// A failed flush dropped the buffer already, failing the reopen would
    /// not bring it back but stop the rotation of the kubelet, which moves
    /// the rotated file back then.
    pub(crate) async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen log file {}", self.path.display());
        if let Err(e) = self.flush().await {
            error!("Unable to flush log file before reopen: {e:#}");
        }
        self.replace().await
    }
}

/// The entry points of the fuzz targets, see `crate::fuzzing`.
#[cfg(feature = "fuzzing")]
pub mod fuzzing;

#[cfg(test)]
mod tests;
