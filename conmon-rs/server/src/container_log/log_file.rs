//! Writing a log file on disk, see [`LogFile`].

use crate::fs_util;
use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    mem,
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::task;
use tracing::{debug, error, trace};

/// A log file on disk which is written in whole chunks.
///
/// The drivers assemble their entries into `buf`, account each one with
/// [`LogFile::commit_entry`] and write the buffer with a single call per read
/// chunk, see [`LogFile::end_chunk`], or per [`LogFile::FLUSH_SIZE`] of
/// entries for chunks of short lines.
/// The writes, the opens and closing a replaced file run on the blocking
/// pool, so writeback throttling, a slow disk or replacing a huge log only
/// stalls this log and not the thread which drives every container of the
/// pod and the RPCs. The buffer moves to the blocking task and back, unlike
/// an async file, which copies every write into a buffer of its own that
/// keeps the size of the largest one.
#[derive(Debug)]
pub(super) struct LogFile {
    path: PathBuf,

    /// The mode of a file this creates, an existing one keeps its own.
    mode: Option<u32>,
    pub(super) file: Option<Arc<File>>,

    /// Maximum allowed log size in bytes.
    max_size: Option<usize>,

    /// The size of the file, counted from the one it had when it got
    /// opened, including the committed entries of `buf`.
    bytes_written: usize,

    /// The pending output of the current chunk, allocated on first use and
    /// released at its end, see [`LogFile::buf`].
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
    /// need with their line overhead, up to about twice
    /// [`LogFile::FLUSH_SIZE`] for the entry which crosses it.
    const BUF_CAPACITY: usize = 8 * 1024;

    /// The entries get written once they take this much, a chunk of very
    /// short lines would grow the buffer to many times its size otherwise.
    const FLUSH_SIZE: usize = 16 * 1024;

    /// A log file at `path`, created with `mode`, which gets replaced once
    /// it grows beyond `max_size`. Nothing gets opened before
    /// [`LogFile::open`].
    pub(super) fn new(path: impl AsRef<Path>, mode: Option<u32>, max_size: Option<usize>) -> Self {
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

    /// The buffer to append the entries of a chunk to. A log file holds one
    /// only while it writes a chunk, see [`LogFile::end_chunk`].
    fn buf(&mut self) -> &mut Vec<u8> {
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
    pub(super) async fn open(&mut self) -> Result<()> {
        debug!("Open log file {}", self.path.display());
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
        let file = task::spawn_blocking(move || Self::create_at(&options, &tmp, &path))
            .await
            .context("join log file replace")??;
        // Closing the old file frees the blocks of a file which got unlinked,
        // which takes long for a huge log. Not awaited, so that the log does
        // not stay locked meanwhile. A sync which still holds it closes it
        // on its own blocking thread.
        if let Some(old) = self.file.replace(Arc::new(file)).and_then(Arc::into_inner) {
            task::spawn_blocking(move || drop(old));
        }
        self.bytes_written = 0;
        self.torn = false;
        Ok(())
    }

    /// Create a new file at `tmp` and rename it to `path`, see
    /// [`LogFile::replace`].
    fn create_at(options: &OpenOptions, tmp: &Path, path: &Path) -> Result<File> {
        let file = options
            .open(tmp)
            .with_context(|| format!("open log file path '{}'", tmp.display()))?;
        if let Err(e) = fs::rename(tmp, path) {
            if let Err(e) = fs::remove_file(tmp) {
                debug!("Unable to remove {}: {e}", tmp.display());
            }
            return Err(e).with_context(|| format!("rename log file to '{}'", path.display()));
        }
        Ok(file)
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
    async fn commit_entry(&mut self, start: usize) -> Result<()> {
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
    /// [`LogFile::commit_entry`], and release the buffer. The line overhead
    /// of the entries grows it beyond [`LogFile::BUF_CAPACITY`] for most
    /// full read chunks, which an idle container would keep otherwise. The
    /// allocation of the next chunk costs little next to its write.
    async fn end_chunk(&mut self) -> Result<()> {
        self.rotation_failed = false;
        let res = self.flush().await;
        self.buf = Vec::new();
        res
    }

    /// Write a chunk of one entry per item, which `entry` appends to the
    /// buffer, and end it. An entry which fails to commit does not stop the
    /// later ones, the first error is returned.
    pub(super) async fn write_entries<T>(
        &mut self,
        items: impl IntoIterator<Item = T>,
        mut entry: impl FnMut(&mut Vec<u8>, T),
    ) -> Result<()> {
        let mut res = Ok(());
        for item in items {
            let buf = self.buf();
            let start = buf.len();
            entry(buf, item);
            res = res.and(self.commit_entry(start).await);
        }
        res.and(self.end_chunk().await)
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
    /// and the data sits in the page cache either way. Only the exit of the
    /// container syncs the log, see
    /// [`ContainerLog::sync`](super::ContainerLog::sync).
    ///
    /// A failed flush dropped the buffer already, failing the reopen would
    /// not bring it back but stop the rotation of the kubelet, which moves
    /// the rotated file back then.
    pub(super) async fn reopen(&mut self) -> Result<()> {
        debug!("Reopen log file {}", self.path.display());
        if let Err(e) = self.flush().await {
            error!("Unable to flush log file before reopen: {e:#}");
        }
        self.replace().await
    }
}

#[cfg(test)]
mod tests;
