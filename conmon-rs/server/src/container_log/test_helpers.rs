//! Setup shared by the tests of the log drivers.

use super::{
    ContainerLog, Driver, LineBuffer, Logger, SharedContainerLog, cri::CriLogger,
    journald::JournaldLogger,
};
use crate::{container_io::Pipe, journal::testing::decode, mutex::lock};
use anyhow::{Context, Result};
use nix::{
    errno::Errno,
    sys::socket::{MsgFlags, recv},
};
use serde_json::json;
use std::{
    fs,
    os::{fd::AsRawFd, unix::net::UnixDatagram},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tempfile::{NamedTempFile, TempDir};

/// The timestamp of every entry a [`cri_logger`] writes.
pub(crate) const TIMESTAMP: &str = "2026-10-20T12:00:00.123456789+02:00";

/// A CRI logger writing to a new temporary file, with a frozen timestamp.
pub(crate) async fn cri_logger(max_size: Option<usize>) -> Result<(NamedTempFile, CriLogger)> {
    let file = NamedTempFile::new()?;
    let mut logger = CriLogger::new(file.path(), max_size);
    logger.freeze_timestamp(TIMESTAMP);
    logger.init().await?;
    Ok((file, logger))
}

/// A socket in place of the one of journald, in a directory of its own.
/// Its entries get read back as one JSON object per entry with its `pipe`,
/// its `message` and whether it is `partial`. They wait on the socket until
/// then, or until it gets dropped with the test. The tests send only a few
/// short ones, which the queue of the socket and the send buffer of the
/// logger, which the tests share, hold without the logger having to wait.
#[derive(Debug)]
pub(crate) struct Journal {
    _dir: TempDir,
    pub(crate) path: PathBuf,
    pub(crate) socket: UnixDatagram,
    /// The entries read back so far.
    entries: Mutex<String>,
}

impl Journal {
    /// More than the largest entry the tests send, a line of the longest
    /// length with the fields of a container, a larger one fails the read
    /// instead of getting cut.
    const MAX_ENTRY: usize = LineBuffer::MAX_LINE + 4096;

    pub(crate) fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("journal.sock");
        let socket = UnixDatagram::bind(&path)?;
        Ok(Self {
            _dir: dir,
            path,
            socket,
            entries: Mutex::default(),
        })
    }

    /// The entries the logger sent so far, see [`Journal`].
    pub(crate) fn read(&self) -> Result<String> {
        let mut entries = lock(&self.entries);
        for entry in receive(&self.socket)? {
            Self::add(&entry, &mut entries)?;
        }
        Ok(entries.clone())
    }

    /// Add an entry as a JSON object.
    fn add(data: &[u8], entries: &mut String) -> Result<()> {
        let fields = decode(data);
        let field = |key| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        let pipe = match field("PRIORITY").map(Vec::as_slice) {
            Some(b"3") => Pipe::StdErr,
            _ => Pipe::StdOut,
        };
        let message = field("MESSAGE").context("entry without a message")?;
        let mut entry = json!({
            "pipe": pipe.as_str(),
            "message": String::from_utf8_lossy(message),
        });
        if field("CONTAINER_PARTIAL_MESSAGE").is_some() {
            entry["partial"] = true.into();
        }
        entries.push_str(&entry.to_string());
        entries.push('\n');
        Ok(())
    }
}

/// The entries which wait on a socket in place of the one of journald,
/// without waiting for more.
pub(crate) fn receive(socket: &UnixDatagram) -> Result<Vec<Vec<u8>>> {
    let mut buf = vec![0; Journal::MAX_ENTRY];
    let mut entries = vec![];
    loop {
        let flags = MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_TRUNC;
        let len = match recv(socket.as_raw_fd(), &mut buf, flags) {
            Ok(len) => len,
            Err(Errno::EAGAIN) => return Ok(entries),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        };
        let entry = buf
            .get(..len)
            .with_context(|| format!("entry of {len} bytes, more than the test journal takes"))?;
        entries.push(entry.to_vec());
    }
}

/// The content of a log file.
pub(crate) fn read(file: &NamedTempFile) -> Result<String> {
    Ok(fs::read_to_string(file.path())?)
}

/// A container log with a CRI driver writing to a new temporary file, with
/// a frozen timestamp, and a journald driver writing to a [`Journal`].
/// Without `init` the CRI driver is not opened and every write fails, the
/// journald one needs no opening.
pub(crate) async fn container_log(
    init: bool,
) -> Result<(NamedTempFile, Journal, SharedContainerLog)> {
    let (cri_file, journal) = (NamedTempFile::new()?, Journal::new()?);
    let mut cri = CriLogger::new(cri_file.path(), None);
    cri.freeze_timestamp(TIMESTAMP);
    let mut log = ContainerLog {
        drivers: vec![
            Driver::Cri(cri),
            Driver::Journald(JournaldLogger::with_socket("id", &journal.path)),
        ],
        lines: LineBuffer::default(),
        buffers_lines: true,
    };
    if init {
        log.init().await?;
    }
    Ok((cri_file, journal, Arc::new(tokio::sync::Mutex::new(log))))
}
