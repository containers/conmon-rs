//! A client for the native journal protocol
//! (<https://systemd.io/JOURNAL_NATIVE_PROTOCOL/>) and a `tracing` writer
//! which forwards the server logs to journald.

use nix::{
    errno::Errno,
    fcntl::{FcntlArg, SealFlag, fcntl},
    sys::{
        memfd::{MFdFlags, memfd_create},
        socket::{ControlMessage, MsgFlags, UnixAddr, sendmsg, setsockopt, sockopt},
    },
};
use std::{
    fs::File,
    io::{self, ErrorKind, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixDatagram,
    },
    path::Path,
    sync::OnceLock,
    time::{Duration, Instant},
};
use tracing::{Level, Metadata};
use tracing_subscriber::fmt::writer::MakeWriter;

/// The datagram socket journald receives native protocol entries on.
pub const SOCKET_PATH: &str = "/run/systemd/journal/socket";

/// The SYSLOG_IDENTIFIER of the entries of the server, and of the containers
/// without a tag, so that `journalctl -t conmonrs` finds them.
pub const IDENTIFIER: &str = "conmonrs";

/// The syslog priorities used for the `PRIORITY` field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Priority {
    Error = 3,
    Warning = 4,
    Notice = 5,
    Info = 6,
    Debug = 7,
}

/// A journal entry, encoded for the native protocol.
#[derive(Debug, Default)]
pub struct Entry(Vec<u8>);

impl Entry {
    /// An entry with the `PRIORITY` and `MESSAGE` fields.
    pub fn new(priority: Priority, message: impl AsRef<[u8]>) -> Self {
        let mut entry = Self::default();
        entry.reset(priority, message);
        entry
    }

    /// Start over with the `PRIORITY` and `MESSAGE` fields, keeping the
    /// allocation.
    pub fn reset(&mut self, priority: Priority, message: impl AsRef<[u8]>) -> &mut Self {
        self.0.clear();
        self.field("PRIORITY", [b'0' + priority as u8])
            .field("MESSAGE", message)
    }

    /// Append a field. The key must be a valid journal field name: upper case
    /// letters, digits and underscores, not starting with a digit or an
    /// underscore.
    pub fn field(&mut self, key: &str, value: impl AsRef<[u8]>) -> &mut Self {
        debug_assert!(Self::valid_key(key), "invalid journal field name {key}");
        let value = value.as_ref();
        self.0.extend_from_slice(key.as_bytes());
        // `KEY=value\n` cannot carry a newline in the value, those values get
        // their length as 64 bit little endian integer instead.
        if value.contains(&b'\n') {
            self.0.push(b'\n');
            self.0
                .extend_from_slice(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
        } else {
            self.0.push(b'=');
        }
        self.0.extend_from_slice(value);
        self.0.push(b'\n');
        self
    }

    /// Whether the key is a valid journal field name, see [`Entry::field`],
    /// at most 64 characters.
    pub fn valid_key(key: &str) -> bool {
        (1..=64).contains(&key.len())
            && !key.starts_with(|c: char| c == '_' || c.is_ascii_digit())
            && key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    }

    /// Append the already encoded fields of another entry.
    pub fn extend(&mut self, fields: &Entry) -> &mut Self {
        self.0.extend_from_slice(&fields.0);
        self
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// How sending waits for a journal which does not keep up, for example while
/// it restarts or its disk is slow. The modes use separate sockets, and the
/// server logs one of their own, see [`SERVER_SOCKET`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wait {
    /// Never block, the entry fails with `ErrorKind::WouldBlock`. For the
    /// server logs, which tracing writes from the runtime threads, and the
    /// container logs as long as the journal takes them right away.
    Never,
    /// Each attempt blocks for at most `SEND_TIMEOUT`, then fails with
    /// `ErrorKind::TimedOut`, one which a signal interrupted gets retried
    /// until the instant. For the container logs which the journal did not
    /// take right away, which get sent from the blocking pool then and
    /// should not be dropped on a short stall.
    Bounded(Instant),
}

/// The longest an attempt of `Wait::Bounded` blocks for one entry.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// The send buffer systemd uses for its journal clients, so that bursts and
/// entries up to that size do not need a memfd.
const SEND_BUFFER_SIZE: usize = 8 * 1024 * 1024;

/// The sockets of the container output, by mode.
static NEVER_SOCKET: OnceLock<UnixDatagram> = OnceLock::new();
static BOUNDED_SOCKET: OnceLock<UnixDatagram> = OnceLock::new();

/// The socket of the server logs. Entries take up the send buffer of the
/// socket they got sent on until the journal received them, so container
/// output which the journal does not take fast enough must not fill the one
/// of the server logs, they would be dropped otherwise.
static SERVER_SOCKET: OnceLock<UnixDatagram> = OnceLock::new();

/// Send an entry of the container output to the journal socket at a path.
pub fn send_at(wait: Wait, path: &Path, entry: &Entry) -> io::Result<()> {
    let cell = match wait {
        Wait::Never => &NEVER_SOCKET,
        Wait::Bounded(_) => &BOUNDED_SOCKET,
    };
    send_on(socket(cell, wait)?, wait, path, entry)
}

/// Send an entry on a socket of the mode, see [`new_socket`].
fn send_on(socket: &UnixDatagram, wait: Wait, path: &Path, entry: &Entry) -> io::Result<()> {
    let retry_until = match wait {
        Wait::Never => None,
        Wait::Bounded(until) => Some(until),
    };
    match send_to(socket, path, entry.as_bytes(), retry_until) {
        Err(e) if retry_until.is_some() && e.kind() == ErrorKind::WouldBlock => {
            Err(io::Error::new(
                ErrorKind::TimedOut,
                format!("journal did not take the entry within an attempt of {SEND_TIMEOUT:?}"),
            ))
        }
        result => result,
    }
}

/// The socket kept in `cell`, created for the mode on first use. A failed
/// creation is not stored, the next entry tries again.
fn socket(cell: &'static OnceLock<UnixDatagram>, wait: Wait) -> io::Result<&'static UnixDatagram> {
    if let Some(socket) = cell.get() {
        return Ok(socket);
    }
    let socket = new_socket(wait)?;
    Ok(cell.get_or_init(|| socket))
}

/// Create a socket which sends the way the mode tells.
fn new_socket(wait: Wait) -> io::Result<UnixDatagram> {
    let socket = UnixDatagram::unbound()?;
    match wait {
        Wait::Never => socket.set_nonblocking(true)?,
        Wait::Bounded(_) => socket.set_write_timeout(Some(SEND_TIMEOUT))?,
    }
    // Best effort like systemd: forcing needs CAP_NET_ADMIN, without it the
    // kernel caps the size at net.core.wmem_max.
    if setsockopt(&socket, sockopt::SndBufForce, &SEND_BUFFER_SIZE).is_err() {
        setsockopt(&socket, sockopt::SndBuf, &SEND_BUFFER_SIZE).ok();
    }
    Ok(socket)
}

/// Send the data as one datagram, or as a memfd if it does not fit.
/// A signal, like the SIGCHLD of every exited child, interrupts a send which
/// waits, the kernel does not restart one with a timeout, so it gets retried
/// until `retry_until`, if any.
fn send_to(
    socket: &UnixDatagram,
    path: &Path,
    data: &[u8],
    retry_until: Option<Instant>,
) -> io::Result<()> {
    let retry = || retry_until.is_none_or(|until| Instant::now() < until);
    loop {
        match socket.send_to(data, path) {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::Interrupted && retry() => {}
            Err(e)
                if matches!(
                    e.raw_os_error().map(Errno::from_raw),
                    Some(Errno::EMSGSIZE | Errno::ENOBUFS)
                ) =>
            {
                return send_memfd(socket, path, data, retry);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Whether an error means that there is no journal to send to: no socket
/// file, or no one listening on it.
pub fn unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::NotFound | ErrorKind::ConnectionRefused
    )
}

/// Send the data in a sealed memfd, the only kind journald accepts, as
/// `SCM_RIGHTS` of an otherwise empty datagram.
fn send_memfd(
    socket: &UnixDatagram,
    path: &Path,
    data: &[u8],
    retry: impl Fn() -> bool,
) -> io::Result<()> {
    let mut file = File::from(create_memfd()?);
    file.write_all(data)?;
    fcntl(
        &file,
        FcntlArg::F_ADD_SEALS(
            SealFlag::F_SEAL_SHRINK
                | SealFlag::F_SEAL_GROW
                | SealFlag::F_SEAL_WRITE
                | SealFlag::F_SEAL_SEAL,
        ),
    )?;
    let addr = UnixAddr::new(path)?;
    loop {
        match sendmsg(
            socket.as_raw_fd(),
            &[],
            &[ControlMessage::ScmRights(&[file.as_raw_fd()])],
            MsgFlags::empty(),
            Some(&addr),
        ) {
            Err(Errno::EINTR) if retry() => {}
            res => return res.map(drop).map_err(Into::into),
        }
    }
}

/// Create a memfd which allows sealing. Like systemd, ask for a
/// non-executable one first, kernels before 6.3 reject that flag.
fn create_memfd() -> nix::Result<OwnedFd> {
    let flags = MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING;
    let noexec = MFdFlags::from_bits_retain(libc::MFD_NOEXEC_SEAL);
    match memfd_create(c"conmonrs-journal", flags | noexec) {
        Err(Errno::EINVAL) => memfd_create(c"conmonrs-journal", flags),
        result => result,
    }
}

/// Writes every log line as one journal entry with the priority derived from
/// the tracing level.
#[derive(Default)]
pub struct Journal {
    priority: Option<Priority>,
}

impl Journal {
    const DEFAULT_PRIORITY: Priority = Priority::Notice;

    /// The entry of a formatted event. The journal separates entries itself,
    /// so the line ending of the event is dropped.
    fn entry(&self, line: &[u8]) -> Entry {
        let mut entry = Entry::new(
            self.priority.unwrap_or(Self::DEFAULT_PRIORITY),
            line.strip_suffix(b"\n").unwrap_or(line),
        );
        entry.field("SYSLOG_IDENTIFIER", IDENTIFIER);
        entry
    }

    fn priority_for(level: Level) -> Priority {
        match level {
            Level::ERROR => Priority::Error,
            Level::WARN => Priority::Warning,
            Level::INFO => Priority::Info,
            Level::DEBUG | Level::TRACE => Priority::Debug,
        }
    }
}

impl Journal {
    /// Send without blocking the runtime thread of the event. A system
    /// without journald is not an error, like with `sd_journal_send`, and a
    /// journal which does not keep up loses the line, otherwise every event
    /// would report an error.
    fn send_at(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        let socket = socket(&SERVER_SOCKET, Wait::Never)?;
        match send_on(socket, Wait::Never, path, &self.entry(line)) {
            Err(e) if unavailable(&e) || e.kind() == ErrorKind::WouldBlock => Ok(()),
            result => result,
        }
    }
}

impl Write for Journal {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send_at(Path::new(SOCKET_PATH), buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Journal {
    type Writer = Journal;

    fn make_writer(&'a self) -> Self::Writer {
        Journal::default()
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        Journal {
            priority: Some(Self::priority_for(*meta.level())),
        }
    }
}

/// The entry point of the fuzz target.
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzzing;

/// What the tests and the fuzz target share.
#[cfg(any(test, feature = "fuzzing"))]
pub mod testing;

#[cfg(test)]
pub(crate) mod tests;
