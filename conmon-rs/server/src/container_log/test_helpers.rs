//! Setup shared by the tests of the log drivers.

use super::{
    ContainerLog, Driver, LineBuffer, Logger, SharedContainerLog, cri::CriLogger,
    journald::JournaldLogger,
};
use crate::{container_io::Pipe, journal::testing::decode, mutex::lock};
use anyhow::{Context, Result, bail};
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::socket::{MsgFlags, recv},
};
use serde_json::json;
use std::{
    fs,
    os::{
        fd::{AsFd, AsRawFd},
        unix::net::UnixDatagram,
    },
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    thread,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::Mutex;

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

/// A socket in place of the one of journald, whose entries a thread of its
/// own receives once they arrive, so that the logger never waits for it.
/// They get read back as one JSON object per entry with its `pipe`, its
/// `message` and whether it is `partial`.
#[derive(Debug)]
pub(crate) struct Journal {
    _dir: TempDir,
    path: PathBuf,
    socket: Arc<UnixDatagram>,
    received: Arc<StdMutex<Received>>,
}

/// What a [`Journal`] received so far.
#[derive(Debug)]
struct Received {
    entries: String,
    /// The first entry which could not be read back, which fails the read.
    error: Option<String>,
    /// The buffer of the receiving, which the lock guards as well.
    buf: Vec<u8>,
}

impl Journal {
    /// More than the largest entry the tests send, a larger one fails the
    /// read instead of getting cut.
    const MAX_ENTRY: usize = 256 * 1024;

    /// How often the thread checks whether the journal got dropped.
    const POLL_TIMEOUT: u16 = 100;

    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("journal.sock");
        let socket = Arc::new(UnixDatagram::bind(&path)?);
        let received = Arc::new(StdMutex::new(Received {
            entries: String::new(),
            error: None,
            buf: vec![0; Self::MAX_ENTRY],
        }));
        let (thread_socket, weak) = (socket.clone(), Arc::downgrade(&received));
        thread::spawn(move || {
            loop {
                // Waits without the lock, the receiving takes it, so that no
                // entry is in flight while a read holds it.
                let mut fds = [PollFd::new(thread_socket.as_fd(), PollFlags::POLLIN)];
                match poll(&mut fds, PollTimeout::from(Self::POLL_TIMEOUT)) {
                    Ok(_) => {}
                    Err(Errno::EINTR) => continue,
                    Err(_) => return,
                }
                let Some(received) = weak.upgrade() else {
                    return;
                };
                if Self::receive(&thread_socket, &mut lock(&received)).is_err() {
                    return;
                }
            }
        });
        Ok(Self {
            _dir: dir,
            path,
            socket,
            received,
        })
    }

    /// Add the entries which wait on the socket, without waiting for more.
    /// The caller holds the lock, so that none is in flight.
    fn receive(socket: &UnixDatagram, received: &mut Received) -> Result<()> {
        let Received {
            entries,
            error,
            buf,
        } = received;
        loop {
            let flags = MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_TRUNC;
            let len = match recv(socket.as_raw_fd(), buf, flags) {
                Ok(len) => len,
                Err(Errno::EAGAIN) => return Ok(()),
                Err(Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            };
            if let Err(e) = Self::add(buf, len, entries) {
                error.get_or_insert_with(|| format!("{e:#}"));
            }
        }
    }

    /// Add the entry of `len` bytes, which `buf` holds unless it was larger.
    fn add(buf: &[u8], len: usize, entries: &mut String) -> Result<()> {
        let data = buf
            .get(..len)
            .with_context(|| format!("entry of {len} bytes, more than the test journal takes"))?;
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

    /// The entries the logger sent so far.
    fn read(&self) -> Result<String> {
        let mut received = lock(&self.received);
        Self::receive(&self.socket, &mut received)?;
        if let Some(e) = &received.error {
            bail!("unable to read back an entry: {e}");
        }
        Ok(received.entries.clone())
    }
}

/// What the tests read back, a log file or a [`Journal`].
pub(crate) trait Readable {
    fn read_all(&self) -> Result<String>;
}

impl Readable for NamedTempFile {
    fn read_all(&self) -> Result<String> {
        Ok(fs::read_to_string(self.path())?)
    }
}

impl Readable for Journal {
    fn read_all(&self) -> Result<String> {
        self.read()
    }
}

/// The content of a log file or a [`Journal`].
pub(crate) fn read(log: &impl Readable) -> Result<String> {
    log.read_all()
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
    Ok((cri_file, journal, Arc::new(Mutex::new(log))))
}
