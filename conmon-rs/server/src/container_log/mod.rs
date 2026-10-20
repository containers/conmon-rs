mod cri;
mod journald;
mod json;
#[cfg(test)]
pub(crate) mod test_helpers;

use crate::conmon_capnp::conmon::log_driver::{self, Owned, Type};
use crate::{
    container_io::Pipe, container_log::cri::CriLogger, container_log::journald::JournaldLogger,
    container_log::json::JsonLogger,
};
use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use capnp::struct_list::Reader;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    mem,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{sync::RwLock, task};
use tracing::{debug, trace};

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
                        Driver::Cri(CriLogger::new(driver.get_path()?.to_str()?, max_size))
                    }
                    Type::Json => {
                        Driver::Json(JsonLogger::new(driver.get_path()?.to_str()?, max_size))
                    }
                    Type::Journald => Driver::Journald(JournaldLogger::new(id)),
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

    /// The maximum log size of a requested driver, zero means unlimited.
    fn max_size(driver: &log_driver::Reader<'_>) -> Option<usize> {
        match driver.get_max_size() {
            0 => None,
            max => Some(max as usize),
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
        let mut end = match str::from_utf8(tail) {
            Ok(_) => tail.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => tail.len(),
        };
        if tail[..end].ends_with(b"\r") {
            end -= 1;
        }
        Line {
            text: tail.split_to(end).freeze(),
            partial: true,
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
        self.take(pipe)
    }

    /// The output of the pipe ended, return the line which still waits for
    /// its ending as a partial one.
    pub(crate) fn finish(&mut self, pipe: Pipe) -> Vec<Line> {
        self.take(pipe)
    }

    fn take(&mut self, pipe: Pipe) -> Vec<Line> {
        let tail = self.tail(pipe);
        if tail.is_empty() {
            return vec![];
        }
        let text = tail.split().freeze();
        Self::shrink(tail);
        vec![Line {
            text: text.slice(..strip_line_ending(&text).len()),
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
/// not the thread which drives every container of the pod. The buffer moves
/// to the blocking task and back, unlike an async file, which copies every
/// write into a buffer of its own that keeps the size of the largest one.
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

    /// The pending output of the current chunk.
    pub(crate) buf: Vec<u8>,
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
            buf: Vec::with_capacity(Self::BUF_CAPACITY),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Open the file, truncating any existing content. A symlink in place of
    /// the log is not followed, the truncation would hit its target.
    pub(crate) async fn open(&mut self) -> Result<()> {
        let options = self.open_options();
        let path = self.path.clone();
        let file = task::spawn_blocking(move || options.open(path))
            .await
            .context("join log file open")?
            .with_context(|| format!("open log file path '{}'", self.path.display()))?;
        self.file = Some(Arc::new(file));
        self.bytes_written = 0;
        Ok(())
    }

    /// Replace the file with a new empty one, like conmon does: create
    /// `<path>.tmp` and rename it over the path. Unlike a truncation in
    /// place, a reader which follows the log, like `kubectl logs -f`, keeps
    /// the old file with its offset and reads it to its end, before it
    /// notices the new one. A rename does not follow a symlink at the path
    /// either.
    async fn replace(&mut self) -> Result<()> {
        let options = self.open_options();
        let path = self.path.clone();
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
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
        Ok(())
    }

    fn open_options(&self) -> OpenOptions {
        let mut options = OpenOptions::new();
        options
            .create(true)
            .read(true)
            .truncate(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW);
        if let Some(mode) = self.mode {
            options.mode(mode);
        }
        options
    }

    /// Account for the entry appended to `buf` since `start`. If it would
    /// exceed the maximum log size, the file gets replaced and the entry
    /// becomes the first one of the new file. The entries of the chunk before
    /// it are dropped instead of written, the rotation would discard them
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
            if let Err(e) = self.replace().await {
                // The old file stays in place. Drop the uncounted entry and
                // write the ones which fit into it now. The next entry tries
                // the rotation again, until then entries which exceed the size
                // get dropped.
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
        if self.buf.len() >= Self::FLUSH_SIZE {
            self.flush().await?;
        }
        Ok(())
    }

    /// Write the pending buffer to the file and clear it.
    pub(crate) async fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let len = self.buf.len();
        let res = self.write_buf().await;
        if res.is_err() {
            // Drop the chunk even if the write failed. It may have been written
            // partially, retrying it along with the next chunk would duplicate
            // that part. Assume it did not make it, so that a later rotation
            // does not happen early.
            self.bytes_written = self.bytes_written.saturating_sub(len);
        }
        self.buf.clear();
        if self.buf.capacity() > Self::BUF_RETAIN_LIMIT {
            self.buf.shrink_to(Self::BUF_CAPACITY);
        }
        res
    }

    async fn write_buf(&mut self) -> Result<()> {
        let file = self.file.clone().context(Self::ERR_UNINITIALIZED)?;
        let buf = mem::take(&mut self.buf);
        let (buf, res) = match task::spawn_blocking(move || {
            let res = (&*file).write_all(&buf);
            (buf, res)
        })
        .await
        {
            Ok(res) => res,
            Err(e) => {
                // The buffer went with the task.
                self.buf = Vec::with_capacity(Self::BUF_CAPACITY);
                return Err(e).context("join log write");
            }
        };
        self.buf = buf;
        res.context("write log chunk")
    }

    /// Write the pending buffer and start a new file at the path. The
    /// kubelet renamed the old one already, a file still at the path gets
    /// replaced, like by the rotation.
    ///
    /// The file is not synced to disk: an fsync of a large log can take long
    /// and the data sits in the page cache either way.
    pub(crate) async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen log file {}", self.path.display());
        self.flush().await.context("flush log file before reopen")?;
        self.replace().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::NamedTempFile;

    fn collect(lines: Vec<Line>) -> Vec<(String, bool)> {
        lines
            .into_iter()
            .map(|line| {
                (
                    String::from_utf8_lossy(&line.text).into_owned(),
                    line.partial,
                )
            })
            .collect()
    }

    #[test]
    fn line_buffer_joins_chunks_per_pipe() {
        let mut buffer = LineBuffer::default();
        assert_eq!(
            collect(buffer.push(Pipe::StdOut, b"first\nab")),
            vec![("first".into(), false)]
        );
        assert!(buffer.has_pending(Pipe::StdOut));
        // The other pipe has its own tail.
        assert!(!buffer.has_pending(Pipe::StdErr));
        assert_eq!(
            collect(buffer.push(Pipe::StdErr, b"err\n")),
            vec![("err".into(), false)]
        );
        assert_eq!(
            collect(buffer.push(Pipe::StdOut, b"cd\nthird")),
            vec![("abcd".into(), false)]
        );

        // The output ended, the rest is a partial line.
        assert_eq!(
            collect(buffer.finish(Pipe::StdOut)),
            vec![("third".into(), true)]
        );
        assert!(buffer.finish(Pipe::StdOut).is_empty());
        assert!(!buffer.has_pending(Pipe::StdOut));
    }

    #[test]
    fn line_buffer_flushes_paused_output() {
        let mut buffer = LineBuffer::default();
        assert!(buffer.push(Pipe::StdOut, b"prompt> ").is_empty());
        assert_eq!(
            collect(buffer.flush(Pipe::StdOut)),
            vec![("prompt> ".into(), true)]
        );
        assert!(buffer.flush(Pipe::StdOut).is_empty());
        // The continuation is its own line.
        assert_eq!(
            collect(buffer.push(Pipe::StdOut, b"ls\n")),
            vec![("ls".into(), false)]
        );
    }

    #[test]
    fn line_buffer_handles_split_line_endings_and_characters() {
        let mut buffer = LineBuffer::default();
        // "\r\n" split between two chunks.
        assert!(buffer.push(Pipe::StdOut, b"terminal\r").is_empty());
        assert_eq!(
            collect(buffer.push(Pipe::StdOut, b"\n")),
            vec![("terminal".into(), false)]
        );

        // A multi byte character split between two chunks.
        let euro = "€".as_bytes();
        assert!(buffer.push(Pipe::StdOut, &euro[..1]).is_empty());
        assert_eq!(
            collect(buffer.push(Pipe::StdOut, &[&euro[1..], b"\n"].concat())),
            vec![("€".into(), false)]
        );
    }

    #[test]
    fn line_buffer_cuts_long_lines() {
        let euro = "€".as_bytes();
        let long = vec![b'a'; LineBuffer::MAX_LINE - 1];

        // An unfinished character at the cut stays buffered.
        let mut buffer = LineBuffer::default();
        let out = buffer.push(Pipe::StdOut, &[long.as_slice(), &euro[..1]].concat());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text.len(), LineBuffer::MAX_LINE - 1);
        assert!(out[0].partial);
        assert_eq!(
            collect(buffer.push(Pipe::StdOut, &[&euro[1..], b"\n"].concat())),
            vec![("€".into(), false)]
        );

        // So does a "\r" which may be followed by its "\n".
        let mut buffer = LineBuffer::default();
        let out = buffer.push(Pipe::StdOut, &[long.as_slice(), b"\r"].concat());
        assert_eq!(out[0].text.len(), LineBuffer::MAX_LINE - 1);
        assert_eq!(
            collect(buffer.push(Pipe::StdOut, b"\n")),
            vec![("".into(), false)]
        );

        // Invalid bytes are not held back, the buffer would grow forever.
        let mut buffer = LineBuffer::default();
        let mut invalid = vec![0xff; LineBuffer::MAX_LINE];
        invalid.push(0xfe);
        let out = buffer.push(Pipe::StdOut, &invalid);
        assert_eq!(out[0].text.len(), invalid.len());
        assert!(!buffer.has_pending(Pipe::StdOut));
        assert!(buffer.stdout.capacity() <= LineBuffer::MAX_LINE);
    }

    #[test]
    fn lines_split_a_chunk() {
        let got: Vec<_> = lines(b"a\nb").collect();
        assert_eq!(got, vec![(&b"a\n"[..], false), (&b"b"[..], true)]);
        assert_eq!(strip_line_ending(b"x\r\n"), b"x");
        assert_eq!(strip_line_ending(b"x\n"), b"x");
        assert_eq!(strip_line_ending(b"x"), b"x");
    }

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

        // Keep the open file reachable, then make the rotation fail by putting
        // a directory in its place.
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
        assert!(!dir.path().join("log.tmp").exists());
        Ok(())
    }

    #[tokio::test]
    async fn rotation_replaces_the_file() -> Result<()> {
        use std::{io::Read, os::unix::fs::MetadataExt};

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("log");
        let mut sut = LogFile::new(&path, Some(0o600), Some(10));
        sut.open().await?;
        sut.buf.extend_from_slice(b"first\n");
        sut.commit_entry(0).await?;
        sut.flush().await?;

        // A reader following the log, like the kubelet.
        let mut reader = File::open(&path)?;
        let mut old = String::new();
        reader.read_to_string(&mut old)?;
        assert_eq!(old, "first\n");
        let ino = fs::metadata(&path)?.ino();

        sut.buf.extend_from_slice(b"second\n");
        sut.commit_entry(0).await?;
        sut.flush().await?;
        let metadata = fs::metadata(&path)?;
        assert_ne!(metadata.ino(), ino);
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&path)?, "second\n");
        assert!(!dir.path().join("log.tmp").exists());

        // The old file keeps its content, the reader is at its end.
        assert_eq!(reader.metadata()?.ino(), ino);
        assert_eq!(reader.read_to_string(&mut old)?, 0);
        assert_eq!(reader.metadata()?.len(), 6);

        // The reopen after the kubelet renamed the file starts a new one.
        let rotated = dir.path().join("log.1");
        fs::rename(&path, &rotated)?;
        sut.reopen().await?;
        sut.buf.extend_from_slice(b"third\n");
        sut.commit_entry(0).await?;
        sut.flush().await?;
        assert_eq!(fs::read_to_string(&rotated)?, "second\n");
        assert_eq!(fs::read_to_string(&path)?, "third\n");
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

    #[tokio::test]
    async fn drivers_share_the_output() -> Result<()> {
        let (cri, json, log) = test_helpers::container_log(true).await?;
        let mut log = log.write().await;

        log.write(Pipe::StdOut, b"a\nparti").await?;
        log.write(Pipe::StdOut, b"al\n").await?;
        log.finish(Pipe::StdOut).await?;

        // The CRI log writes partial lines as they come, the JSON one joins
        // them.
        let ts = test_helpers::TIMESTAMP;
        assert_eq!(
            test_helpers::read(&cri)?,
            format!("{ts} stdout F a\n{ts} stdout P parti\n{ts} stdout F al\n")
        );
        let json = test_helpers::read(&json)?;
        let messages: Vec<_> = json
            .lines()
            .map(|line| {
                let entry: serde_json::Value = serde_json::from_str(line)?;
                Ok(entry["message"].as_str().unwrap_or_default().to_owned())
            })
            .collect::<Result<_>>()?;
        assert_eq!(messages, ["a", "partial"]);
        Ok(())
    }

    #[tokio::test]
    async fn failing_driver_does_not_stop_the_others() -> Result<()> {
        let (cri, json, log) = test_helpers::container_log(false).await?;
        let mut log = log.write().await;
        // Only the JSON log gets opened, the CRI one fails every write.
        if let Driver::Json(json) = &mut log.drivers[1] {
            json.init().await?;
        }

        assert!(log.write(Pipe::StdOut, b"line\n").await.is_err());
        assert!(test_helpers::read(&cri)?.is_empty());
        assert!(test_helpers::read(&json)?.contains(r#""message":"line""#));
        Ok(())
    }

    #[tokio::test]
    async fn reopen_reopens_all_drivers() -> Result<()> {
        let (cri, json, log) = test_helpers::container_log(true).await?;
        let mut log = log.write().await;
        log.write(Pipe::StdOut, b"before\n").await?;

        // The CRI log cannot be opened again.
        fs::remove_file(cri.path())?;
        fs::create_dir(cri.path())?;

        assert!(log.reopen().await.is_err());
        assert!(test_helpers::read(&json)?.is_empty());
        fs::remove_dir(cri.path())?;
        Ok(())
    }

    #[tokio::test]
    async fn open_does_not_follow_symlinks() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("target");
        fs::write(&target, "keep")?;
        let link = dir.path().join("log");
        std::os::unix::fs::symlink(&target, &link)?;

        let mut sut = LogFile::new(&link, Some(0o600), None);
        assert!(sut.open().await.is_err());
        assert_eq!(fs::read_to_string(&target)?, "keep");
        Ok(())
    }

    #[tokio::test]
    async fn short_lines_are_written_in_bounded_pieces() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut sut = LogFile::new(file.path(), None, None);
        sut.open().await?;

        let mut expected = String::new();
        for i in 0..10_000 {
            let start = sut.buf.len();
            let entry = format!("{i}\n");
            sut.buf.extend_from_slice(entry.as_bytes());
            expected.push_str(&entry);
            sut.commit_entry(start).await?;
            assert!(sut.buf.len() < LogFile::FLUSH_SIZE);
        }
        sut.flush().await?;
        assert!(sut.buf.capacity() <= LogFile::BUF_RETAIN_LIMIT);
        assert_eq!(fs::read_to_string(file.path())?, expected);
        Ok(())
    }
}
