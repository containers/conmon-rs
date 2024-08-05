use crate::errors::{Context, SdError};
use nix::errno::Errno;
use nix::fcntl::*;
use nix::sys::memfd::MemFdCreateFlag;
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags, UnixAddr};
use nix::sys::stat::{fstat, FileStat};
use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io::prelude::*;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::os::unix::prelude::AsFd;
use std::os::unix::prelude::FromRawFd;
use std::os::unix::prelude::RawFd;
use std::str::FromStr;

/// Default path of the systemd-journald `AF_UNIX` datagram socket.
pub static SD_JOURNAL_SOCK_PATH: &str = "/run/systemd/journal/socket";

/// The shared socket to journald.
static SD_SOCK: OnceCell<UnixDatagram> = OnceCell::new();

/// Well-known field names.  Their validity is covered in tests.
const PRIORITY: ValidField = ValidField::unchecked("PRIORITY");
const MESSAGE: ValidField = ValidField::unchecked("MESSAGE");

/// Trait for checking the type of a file descriptor.

/// Log priority values.
///
/// See `man 3 syslog`.
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum Priority {
    /// System is unusable.
    Emergency = 0,
    /// Action must be taken immediately.
    Alert,
    /// Critical condition,
    Critical,
    /// Error condition.
    Error,
    /// Warning condition.
    Warning,
    /// Normal, but significant, condition.
    Notice,
    /// Informational message.
    Info,
    /// Debug message.
    Debug,
}

impl std::convert::From<Priority> for u8 {
    fn from(p: Priority) -> Self {
        match p {
            Priority::Emergency => 0,
            Priority::Alert => 1,
            Priority::Critical => 2,
            Priority::Error => 3,
            Priority::Warning => 4,
            Priority::Notice => 5,
            Priority::Info => 6,
            Priority::Debug => 7,
        }
    }
}

impl Priority {
    fn numeric_level(&self) -> &str {
        match self {
            Priority::Emergency => "0",
            Priority::Alert => "1",
            Priority::Critical => "2",
            Priority::Error => "3",
            Priority::Warning => "4",
            Priority::Notice => "5",
            Priority::Info => "6",
            Priority::Debug => "7",
        }
    }
}

#[inline(always)]
fn is_valid_char(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'
}

/// The variable name must be in uppercase and consist only of characters,
/// numbers and underscores, and may not begin with an underscore.
///
/// See <https://github.com/systemd/systemd/blob/ed056c560b47f84a0aa0289151f4ec91f786d24a/src/libsystemd/sd-journal/journal-file.c#L1557>
/// for the reference implementation of journal_field_valid.
fn is_valid_field(input: &str) -> bool {
    // journald doesn't allow empty fields or fields with more than 64 bytes
    if input.is_empty() || 64 < input.len() {
        return false;
    }

    // Fields starting with underscores are protected by journald
    if input.starts_with('_') {
        return false;
    }

    // Journald doesn't allow fields to start with digits
    if input.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }

    input.chars().all(is_valid_char)
}

/// A helper for functions that want to take fields as parameters that have already been validated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ValidField<'a> {
    field: &'a str,
}

impl<'a> ValidField<'a> {
    /// The field value is checked by [[`is_valid_field`]] and a ValidField is returned if true.
    fn validate(field: &'a str) -> Option<Self> {
        if is_valid_field(field) {
            Some(Self { field })
        } else {
            None
        }
    }

    /// Allows for the construction of a potentially invalid ValidField.
    ///
    /// Since [[`ValidField::is_valid_field`]] cannot reasonably be const, this allows for the
    /// construction of known valid field names at compile time.  It's expected that the validity is
    /// confirmed in tests by [[`ValidField::validate_unchecked`]].
    const fn unchecked(field: &'a str) -> Self {
        Self { field }
    }

    /// Converts to a byte slice.
    fn as_bytes(&self) -> &'a [u8] {
        self.field.as_bytes()
    }

    /// Returns the length in bytes.
    fn len(&self) -> usize {
        self.field.len()
    }

    /// Validates an object created using [[`ValidField::unchecked`]].
    ///
    /// Every unchecked field should have a corresponding test that calls this.
    #[cfg(test)]
    fn validate_unchecked(&self) -> bool {
        is_valid_field(self.field)
    }
}

/// Add `field` and `payload` to journal fields `data` with explicit length encoding.
///
/// Write
///
/// 1. the field name,
/// 2. an ASCII newline,
/// 3. the payload size as LE encoded 64 bit integer,
/// 4. the payload, and
/// 5. a final ASCII newline
///
/// to `data`.
///
/// See <https://systemd.io/JOURNAL_NATIVE_PROTOCOL/> for details.
fn add_field_and_payload_explicit_length(data: &mut Vec<u8>, field: ValidField, payload: &str) {
    let encoded_len = (payload.len() as u64).to_le_bytes();

    // Bump the capacity to avoid multiple allocations during the extend/push calls.  The 2 is for
    // the newline characters.
    data.reserve(field.len() + encoded_len.len() + payload.len() + 2);

    data.extend(field.as_bytes());
    data.push(b'\n');
    data.extend(encoded_len);
    data.extend(payload.as_bytes());
    data.push(b'\n');
}

/// Add  a journal `field` and its `payload` to journal fields `data` with appropriate encoding.
///
/// If `payload` does not contain a newline character use the simple journal field encoding, and
/// write the field name and the payload separated by `=` and suffixed by a final new line.
///
/// Otherwise encode the payload length explicitly with [[`add_field_and_payload_explicit_length`]].
///
/// See <https://systemd.io/JOURNAL_NATIVE_PROTOCOL/> for details.
fn add_field_and_payload(data: &mut Vec<u8>, field: ValidField, payload: &str) {
    if payload.contains('\n') {
        add_field_and_payload_explicit_length(data, field, payload);
    } else {
        // If payload doesn't contain an newline directly write the field name and the payload. Bump
        // the capacity to avoid multiple allocations during extend/push calls.  The 2 is for the
        // two pushed bytes.
        data.reserve(field.len() + payload.len() + 2);

        data.extend(field.as_bytes());
        data.push(b'=');
        data.extend(payload.as_bytes());
        data.push(b'\n');
    }
}

/// Send a message with structured properties to the journal.
///
/// The PRIORITY or MESSAGE fields from the vars iterator are always ignored in favour of the priority and message arguments.
pub fn journal_send<K, V>(
    priority: Priority,
    msg: &str,
    vars: impl Iterator<Item = (K, V)>,
) -> Result<(), SdError>
where
    K: AsRef<str>,
    V: AsRef<str>,
{
    let sock = SD_SOCK
        .get_or_try_init(UnixDatagram::unbound)
        .context("failed to open datagram socket")?;

    let mut data = Vec::new();
    add_field_and_payload(&mut data, PRIORITY, priority.numeric_level());
    add_field_and_payload(&mut data, MESSAGE, msg);
    for (ref k, ref v) in vars {
        if let Some(field) = ValidField::validate(k.as_ref()) {
            if field != PRIORITY && field != MESSAGE {
                add_field_and_payload(&mut data, field, v.as_ref())
            }
        }
    }

    // Message sending logic:
    //  * fast path: data within datagram body.
    //  * slow path: data in a sealed memfd, which is sent as an FD in ancillary data.
    //
    // Maximum data size is system dependent, thus this always tries the fast path and
    // falls back to the slow path if the former fails with `EMSGSIZE`.
    match sock.send_to(&data, SD_JOURNAL_SOCK_PATH) {
        Ok(x) => Ok(x),
        // `EMSGSIZE` (errno code 90) means the message was too long for a UNIX socket,
        Err(ref err) if err.raw_os_error() == Some(90) => {
            send_memfd_payload(sock, &data).context("sending with memfd failed")
        }
        Err(e) => Err(e).context("send_to failed"),
    }
    .map(|_| ())
    .with_context(|| format!("failed to print to journal at '{}'", SD_JOURNAL_SOCK_PATH))
}

/// Print a message to the journal with the given priority.
pub fn journal_print(priority: Priority, msg: &str) -> Result<(), SdError> {
    let map: HashMap<&str, &str> = HashMap::new();
    journal_send(priority, msg, map.iter())
}

// Implementation of memfd_create() using a syscall instead of calling the libc
// function.
//
// The memfd_create() function is only available in glibc >= 2.27 (and other
// libc implementations). To support older versions of glibc, we perform a raw
// syscall (this will fail in Linux < 3.17, where the syscall was not
// available).
//
// nix::sys::memfd::memfd_create chooses at compile time between calling libc
// and performing a syscall, since platforms such as Android and uclibc don't
// have memfd_create() in libc. Here we always use the syscall.
fn memfd_create(name: &CStr, flags: MemFdCreateFlag) -> Result<File, Errno> {
    unsafe {
        let res = libc::syscall(libc::SYS_memfd_create, name.as_ptr(), flags.bits());
        Errno::result(res).map(|r| {
            // SAFETY: `memfd_create` just returned this FD, so we own it now.
            File::from_raw_fd(r as RawFd)
        })
    }
}

/// Send an overlarge payload to systemd-journald socket.
///
/// This is a slow-path for sending a large payload that could not otherwise fit
/// in a UNIX datagram. Payload is thus written to a memfd, which is sent as ancillary
/// data.
fn send_memfd_payload(sock: &UnixDatagram, data: &[u8]) -> Result<usize, SdError> {
    let memfd = {
        let fdname = &CString::new("libsystemd-rs-logging").context("unable to create cstring")?;
        let mut file = memfd_create(fdname, MemFdCreateFlag::MFD_ALLOW_SEALING)
            .context("unable to create memfd")?;

        file.write_all(data).context("failed to write to memfd")?;
        file
    };

    // Seal the memfd, so that journald knows it can safely mmap/read it.
    fcntl(memfd.as_raw_fd(), FcntlArg::F_ADD_SEALS(SealFlag::all()))
        .context("unable to seal memfd")?;

    let fds = &[memfd.as_raw_fd()];
    let ancillary = [ControlMessage::ScmRights(fds)];
    let path = UnixAddr::new(SD_JOURNAL_SOCK_PATH).context("unable to create new unix address")?;
    sendmsg(
        sock.as_raw_fd(),
        &[],
        &ancillary,
        MsgFlags::empty(),
        Some(&path),
    )
    .context("sendmsg failed")?;

    // Close our side of the memfd after we send it to systemd.
    drop(memfd);

    Ok(data.len())
}

/// A systemd journal stream.
#[derive(Debug, Eq, PartialEq)]
pub struct JournalStream {
    /// The device number of the journal stream.
    device: libc::dev_t,
    /// The inode number of the journal stream.
    inode: libc::ino_t,
}

impl JournalStream {
    /// Parse the device and inode number from a systemd journal stream specification.
    ///
    /// See also [`JournalStream::from_env()`].
    pub(crate) fn parse<S: AsRef<OsStr>>(value: S) -> Result<Self, SdError> {
        let s = value.as_ref().to_str().with_context(|| {
            format!(
                "Failed to parse journal stream: Value {:?} not UTF-8 encoded",
                value.as_ref()
            )
        })?;
        let (device_s, inode_s) =
            s.find(':')
                .map(|i| (&s[..i], &s[i + 1..]))
                .with_context(|| {
                    format!(
                        "Failed to parse journal stream: Missing separator ':' in value '{}'",
                        s
                    )
                })?;
        let device = libc::dev_t::from_str(device_s).with_context(|| {
            format!(
                "Failed to parse journal stream: Device part is not a number '{}'",
                device_s
            )
        })?;
        let inode = libc::ino_t::from_str(inode_s).with_context(|| {
            format!(
                "Failed to parse journal stream: Inode part is not a number '{}'",
                inode_s
            )
        })?;
        Ok(JournalStream { device, inode })
    }

    /// Parse the device and inode number of the systemd journal stream denoted by the given environment variable.
    pub(crate) fn from_env_impl<S: AsRef<OsStr>>(key: S) -> Result<Self, SdError> {
        Self::parse(std::env::var_os(&key).with_context(|| {
            format!(
                "Failed to parse journal stream: Environment variable {:?} unset",
                key.as_ref()
            )
        })?)
    }

    /// Parse the device and inode number of the systemd journal stream denoted by the default `$JOURNAL_STREAM` variable.
    ///
    /// These values are extracted from `$JOURNAL_STREAM`, and consists of the device and inode
    /// numbers of the systemd journal stream, separated by `:`.
    pub fn from_env() -> Result<Self, SdError> {
        Self::from_env_impl("JOURNAL_STREAM")
    }

    /// Get the journal stream that would correspond to the given file descriptor.
    ///
    /// Return a journal stream struct containing the device and inode number of the given file descriptor.
    pub fn from_fd<F: AsFd>(fd: F) -> std::io::Result<Self> {
        fstat(fd.as_fd().as_raw_fd())
            .map_err(Into::into)
            .map(Into::into)
    }
}

impl From<FileStat> for JournalStream {
    fn from(stat: FileStat) -> Self {
        Self {
            device: stat.st_dev,
            inode: stat.st_ino,
        }
    }
}

/// Whether this process can be automatically upgraded to native journal logging.
///
/// Inspects the `$JOURNAL_STREAM` environment variable and compares the device and inode
/// numbers in this variable against the stderr file descriptor.
///
/// Return `true` if they match, and `false` otherwise (or in case of any IO error).
///
/// For services normally logging to stderr but also supporting systemd-style structured
/// logging, it is recommended to perform this check and then upgrade to the native systemd
/// journal protocol if possible.
///
/// See section “Automatic Protocol Upgrading” in [systemd documentation][1] for more information.
///
/// [1]: https://systemd.io/JOURNAL_NATIVE_PROTOCOL/#automatic-protocol-upgrading
pub fn connected_to_journal() -> bool {
    JournalStream::from_env().map_or(false, |env_stream| {
        JournalStream::from_fd(std::io::stderr()).map_or(false, |o| o == env_stream)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_journald_socket() -> bool {
        match std::fs::metadata(SD_JOURNAL_SOCK_PATH) {
            Ok(_) => true,
            Err(_) => {
                eprintln!(
                    "skipped, journald socket not found at '{}'",
                    SD_JOURNAL_SOCK_PATH
                );
                false
            }
        }
    }

    const FOO: ValidField = ValidField::unchecked("FOO");

    #[test]
    fn test_priority_numeric_level_matches_to_string() {
        let priorities = [
            Priority::Emergency,
            Priority::Alert,
            Priority::Critical,
            Priority::Error,
            Priority::Warning,
            Priority::Notice,
            Priority::Info,
            Priority::Debug,
        ];

        for priority in priorities.into_iter() {
            assert_eq!(&(u8::from(priority)).to_string(), priority.numeric_level());
        }
    }

    #[test]
    fn test_journal_print_simple() {
        if !ensure_journald_socket() {
            return;
        }

        journal_print(Priority::Info, "TEST LOG!").unwrap();
    }

    #[test]
    fn test_journal_print_large_buffer() {
        if !ensure_journald_socket() {
            return;
        }

        let data = "A".repeat(212995);
        journal_print(Priority::Debug, &data).unwrap();
    }

    #[test]
    fn test_journal_send_simple() {
        if !ensure_journald_socket() {
            return;
        }

        let mut map: HashMap<&str, &str> = HashMap::new();
        map.insert("TEST_JOURNALD_LOG1", "foo");
        map.insert("TEST_JOURNALD_LOG2", "bar");
        journal_send(Priority::Info, "Test Journald Log", map.iter()).unwrap()
    }
    #[test]
    fn test_journal_skip_fields() {
        if !ensure_journald_socket() {
            return;
        }

        let mut map: HashMap<&str, &str> = HashMap::new();
        let priority = format!("{}", u8::from(Priority::Warning));
        map.insert("TEST_JOURNALD_LOG3", "result");
        map.insert("PRIORITY", &priority);
        map.insert("MESSAGE", "Duplicate value");
        journal_send(Priority::Info, "Test Skip Fields", map.iter()).unwrap()
    }

    #[test]
    fn test_predeclared_fields_are_valid() {
        assert!(PRIORITY.validate_unchecked());
        assert!(MESSAGE.validate_unchecked());
        assert!(FOO.validate_unchecked());
    }

    #[test]
    fn test_is_valid_field_lowercase_invalid() {
        let field = "test";
        assert!(ValidField::validate(field).is_none());
    }

    #[test]
    fn test_is_valid_field_uppercase_non_ascii_invalid() {
        let field = "TRÖT";
        assert!(ValidField::validate(field).is_none());
    }

    #[test]
    fn test_is_valid_field_uppercase_valid() {
        let field = "TEST";
        assert_eq!(
            ValidField::validate(field).unwrap().as_bytes(),
            field.as_bytes()
        );
    }

    #[test]
    fn test_is_valid_field_uppercase_non_alpha_invalid() {
        let field = "TE!ST";
        assert!(ValidField::validate(field).is_none());
    }

    #[test]
    fn test_is_valid_field_uppercase_leading_underscore_invalid() {
        let field = "_TEST";
        assert!(ValidField::validate(field).is_none());
    }

    #[test]
    fn test_is_valid_field_uppercase_leading_digit_invalid() {
        let field = "1TEST";
        assert!(ValidField::validate(field).is_none());
    }

    #[test]
    fn add_field_and_payload_explicit_length_simple() {
        let mut data = Vec::new();
        add_field_and_payload_explicit_length(&mut data, FOO, "BAR");
        assert_eq!(
            data,
            vec![b'F', b'O', b'O', b'\n', 3, 0, 0, 0, 0, 0, 0, 0, b'B', b'A', b'R', b'\n']
        );
    }

    #[test]
    fn add_field_and_payload_explicit_length_internal_newline() {
        let mut data = Vec::new();
        add_field_and_payload_explicit_length(&mut data, FOO, "B\nAR");
        assert_eq!(
            data,
            vec![b'F', b'O', b'O', b'\n', 4, 0, 0, 0, 0, 0, 0, 0, b'B', b'\n', b'A', b'R', b'\n']
        );
    }

    #[test]
    fn add_field_and_payload_explicit_length_trailing_newline() {
        let mut data = Vec::new();
        add_field_and_payload_explicit_length(&mut data, FOO, "BAR\n");
        assert_eq!(
            data,
            vec![b'F', b'O', b'O', b'\n', 4, 0, 0, 0, 0, 0, 0, 0, b'B', b'A', b'R', b'\n', b'\n']
        );
    }

    #[test]
    fn add_field_and_payload_simple() {
        let mut data = Vec::new();
        add_field_and_payload(&mut data, FOO, "BAR");
        assert_eq!(data, "FOO=BAR\n".as_bytes());
    }

    #[test]
    fn add_field_and_payload_internal_newline() {
        let mut data = Vec::new();
        add_field_and_payload(&mut data, FOO, "B\nAR");
        assert_eq!(
            data,
            vec![b'F', b'O', b'O', b'\n', 4, 0, 0, 0, 0, 0, 0, 0, b'B', b'\n', b'A', b'R', b'\n']
        );
    }

    #[test]
    fn add_field_and_payload_trailing_newline() {
        let mut data = Vec::new();
        add_field_and_payload(&mut data, FOO, "BAR\n");
        assert_eq!(
            data,
            vec![b'F', b'O', b'O', b'\n', 4, 0, 0, 0, 0, 0, 0, 0, b'B', b'A', b'R', b'\n', b'\n']
        );
    }

    #[test]
    fn journal_stream_from_fd_does_not_claim_ownership_of_fd() {
        // Just get hold of some open file which we know exists and can be read by the current user.
        let file = File::open(file!()).unwrap();
        let journal_stream = JournalStream::from_fd(&file).unwrap();
        assert_ne!(journal_stream.device, 0);
        assert_ne!(journal_stream.inode, 0);
        // Easy way to check if a file descriptor is still open, see https://stackoverflow.com/a/12340730/355252
        let result = fcntl(file.as_raw_fd(), FcntlArg::F_GETFD);
        assert!(
            result.is_ok(),
            "File descriptor not valid anymore after JournalStream::from_fd: {:?}",
            result,
        );
    }
}
