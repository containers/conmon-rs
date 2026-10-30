//! The log drivers of the containers, which write the container output to
//! the CRI or JSON log files or to journald.

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
    fs_util,
};
use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use capnp::struct_list::Reader;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    mem,
    ops::Deref,
    os::unix::fs::{FileExt, OpenOptionsExt},
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
    async fn write_lines(&mut self, _: Pipe, _: &[Line<'_>]) -> Result<()> {
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

    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line<'_>]) -> Result<()> {
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
    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line<'_>]) -> Result<()> {
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

    /// Add a chunk and return the lines it completes. The whole lines of the
    /// chunk borrow it, only the end of a line which started in an earlier
    /// chunk and the start of one which continues in a later one get copied
    /// into the buffer of the pipe.
    pub(crate) fn push<'a>(&mut self, pipe: Pipe, bytes: &'a [u8]) -> Vec<Line<'a>> {
        let tail = self.tail(pipe);
        let mut lines = vec![];
        let mut rest = bytes;
        if !tail.is_empty() {
            let end = memchr::memchr(b'\n', rest).map_or(rest.len(), |end| end + 1);
            let (end_of_line, after) = rest.split_at(end);
            tail.extend_from_slice(end_of_line);
            rest = after;
            if tail.ends_with(b"\n") {
                let line = tail.split().freeze();
                lines.push(Line {
                    text: Text::Buffered(line.slice(..strip_line_ending(&line).len())),
                    partial: false,
                });
            }
        }

        while let Some(end) = memchr::memchr(b'\n', rest) {
            let (line, after) = rest.split_at(end + 1);
            lines.push(Line {
                text: Text::Chunk(strip_line_ending(line)),
                partial: false,
            });
            rest = after;
        }
        tail.extend_from_slice(rest);
        lines.extend(Self::cut_long(tail));
        lines
    }

    /// Cut a line which grew too long without its ending, see
    /// [`LineBuffer::cut`], and release the buffer once empty.
    fn cut_long(tail: &mut BytesMut) -> Option<Line<'static>> {
        let line = (tail.len() >= Self::MAX_LINE).then(|| Self::cut(tail));
        Self::shrink(tail);
        line
    }

    /// Cut the tail at a point which keeps an unfinished multi byte character
    /// and a "\r" waiting for its "\n" in the buffer. Invalid bytes are not
    /// held back, they would stay in front forever.
    fn cut(tail: &mut BytesMut) -> Line<'static> {
        let mut end = Self::complete_len(tail);
        if tail[..end].ends_with(b"\r") {
            end -= 1;
        }
        Line {
            text: Text::Buffered(tail.split_to(end).freeze()),
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
    pub(crate) fn flush(&mut self, pipe: Pipe) -> Vec<Line<'static>> {
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
    pub(crate) fn finish(&mut self, pipe: Pipe) -> Vec<Line<'static>> {
        let tail = self.tail(pipe);
        if tail.is_empty() {
            return vec![];
        }
        // Without a newline, so there is no line ending to strip.
        let text = Text::Buffered(tail.split().freeze());
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
/// chunk, see [`LogFile::end_chunk`], or per [`LogFile::FLUSH_SIZE`] of
/// entries for chunks of short lines.
/// The writes and the opens run on the blocking pool, so writeback
/// throttling, a slow disk or replacing a huge log only stalls this log and
/// not the thread which drives every container of the pod and the RPCs. The
/// buffer moves to the blocking task and back, unlike an async file, which
/// copies every write into a buffer of its own that keeps the size of the
/// largest one.
#[derive(Debug)]
pub(crate) struct LogFile {
    path: PathBuf,

    /// The mode of a file this creates, an existing one keeps its own.
    mode: Option<u32>,
    file: Option<Arc<File>>,

    /// Maximum allowed log size in bytes.
    max_size: Option<usize>,

    /// The size of the file, counted from the one it had when it got
    /// opened, including the committed entries of `buf`.
    bytes_written: usize,

    /// The pending output of the current chunk, allocated on first use, see
    /// [`LogFile::buf`].
    buf: Vec<u8>,

    /// Whether a failed write left the file in the middle of an entry, or it
    /// had such an end when it got opened, so that the next entry has to
    /// start on a line of its own.
    torn: bool,

    /// Whether the rotation failed during the current chunk, see
    /// [`LogFile::commit_entry`].
    rotation_failed: bool,
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
            rotation_failed: false,
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

    /// Open the file and append to its existing content, like conmon does.
    /// The content counts for the maximum size, the first entry which
    /// exceeds it rotates the file. Content which does not end with a
    /// newline, like of a server which crashed in the middle of a write,
    /// counts as torn, see [`LogFile::write_buf`]. A symlink in place of the
    /// log is not followed, the writes would hit its target. A temporary file
    /// which a crashed server left behind gets removed, the kubelet would
    /// never clean it up.
    pub(crate) async fn open(&mut self) -> Result<()> {
        let options = self.open_options(false);
        let path = self.path.clone();
        let (file, torn) = task::spawn_blocking(move || {
            let tmp = Self::tmp_path(&path);
            if let Err(e) = fs_util::remove_file(&tmp) {
                debug!("Unable to remove {}: {e}", tmp.display());
            }
            let file = options.open(path)?;
            let torn = Self::ends_torn(&file);
            io::Result::Ok((file, torn))
        })
        .await
        .context("join log file open")?
        .with_context(|| format!("open log file path '{}'", self.path.display()))?;
        self.file = Some(Arc::new(file));
        self.bytes_written = self.file_size().unwrap_or_default();
        self.torn = torn;
        Ok(())
    }

    /// Whether the file has content which does not end with a newline. One
    /// which cannot be read counts as complete.
    fn ends_torn(file: &File) -> bool {
        let Some(last) = file.metadata().ok().and_then(|m| m.len().checked_sub(1)) else {
            return false;
        };
        let mut byte = [0];
        matches!(file.read_at(&mut byte, last), Ok(1)) && byte != *b"\n"
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
        // The new file starts empty, also over a temporary file left behind.
        let options = self.open_options(true);
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
    fn open_options(&self, truncate: bool) -> OpenOptions {
        let mut options = OpenOptions::new();
        options
            .create(true)
            .read(true)
            .truncate(truncate)
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
    ///
    /// An error drops only this entry, or the entries of a failed write, the
    /// writers still commit the later entries of the chunk and report the
    /// first error once they ended it.
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
            if self.rotation_failed {
                // A full disk would fail it again for every entry, the next
                // chunk tries it again.
                trace!("Dropping log entry which exceeds the size");
                self.buf.truncate(start);
                return Ok(());
            }
            debug!("Rotate log file {}", self.path.display());
            let entry = self.buf.split_off(start);
            if let Err(e) = self.flush().await {
                error!("Unable to flush log file before rotation: {e:#}");
            }
            if let Err(e) = self.replace().await {
                // The old file stays in place. Drop the uncounted entry, the
                // next chunk tries the rotation again, until then entries
                // which exceed the size get dropped.
                self.rotation_failed = true;
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

    /// Write the entries of the chunk which are still pending, see
    /// [`LogFile::commit_entry`].
    pub(crate) async fn end_chunk(&mut self) -> Result<()> {
        self.rotation_failed = false;
        self.flush().await
    }

    /// The size of the open file. A stat of an open file does not wait for
    /// the disk, unlike its writes.
    fn file_size(&self) -> Option<usize> {
        let metadata = self.file.as_ref()?.metadata().ok()?;
        usize::try_from(metadata.len()).ok()
    }

    /// Write the pending buffer to the file and clear it. The writers end
    /// their chunks with [`LogFile::end_chunk`] instead.
    async fn flush(&mut self) -> Result<()> {
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
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzzing;

#[cfg(test)]
mod tests;
