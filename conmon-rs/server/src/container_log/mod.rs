mod cri;
mod journald;
mod json;

use crate::conmon_capnp::conmon::log_driver::{self, Owned, Type};
use crate::{
    container_io::Pipe, container_log::cri::CriLogger, container_log::journald::JournaldLogger,
    container_log::json::JsonLogger,
};
use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use capnp::struct_list::Reader;
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

    /// Start over, for example truncate the file for log rotation.
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

    /// Reopen the container logs.
    pub async fn reopen(&mut self) -> Result<()> {
        for driver in &mut self.drivers {
            driver.reopen().await?;
        }
        Ok(())
    }

    /// Write the contents of the provided bytes into all loggers.
    pub async fn write(&mut self, pipe: Pipe, bytes: &[u8]) -> Result<()> {
        for driver in &mut self.drivers {
            driver.write_chunk(pipe, bytes).await?;
        }
        if self.buffers_lines {
            let lines = self.lines.push(pipe, bytes);
            self.write_lines(pipe, &lines).await?;
        }
        Ok(())
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
