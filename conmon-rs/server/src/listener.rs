//! Helpers for the listening sockets of the server: private sockets, socket
//! paths of any length and the errors of the accept loops.

use anyhow::{Context, Result};
use nix::sys::{
    socket::{AddressFamily, Backlog, SockFlag, SockType, UnixAddr, bind, listen, socket},
    stat::{Mode, fchmod},
};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    os::{fd::OwnedFd, unix::io::AsRawFd},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::time;
use tracing::{debug, error};

/// How long an accept loop pauses after a failed accept.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// How often the failed accepts of a listener get logged as errors, a
/// server out of descriptors would log one every [`ACCEPT_BACKOFF`]
/// otherwise.
const ACCEPT_ERROR_INTERVAL: Duration = Duration::from_secs(10);

/// The failed accepts of a listener which keeps going.
#[derive(Debug, Default)]
pub struct AcceptErrors {
    /// When the last one got logged as an error.
    last_logged: Option<Instant>,
}

impl AcceptErrors {
    /// Handle a failed accept. Most errors, like running out of descriptors,
    /// fail every accept until something changes, so pause instead of
    /// spinning. Only a connection which failed on its own is no reason to
    /// wait, or to tell about it beyond the debug log.
    pub async fn failed(&mut self, e: &io::Error) {
        if connection_failed(e) {
            debug!("Unable to accept connection: {e}");
            return;
        }
        if self.log_error_now(Instant::now()) {
            error!("Unable to accept connection: {e}");
        } else {
            debug!("Unable to accept connection: {e}");
        }
        time::sleep(ACCEPT_BACKOFF).await;
    }

    /// Whether an error is due to be logged as one, at most once per
    /// [`ACCEPT_ERROR_INTERVAL`].
    fn log_error_now(&mut self, now: Instant) -> bool {
        if self
            .last_logged
            .is_some_and(|last| now.saturating_duration_since(last) < ACCEPT_ERROR_INTERVAL)
        {
            return false;
        }
        self.last_logged = Some(now);
        true
    }
}

/// Whether an accept failed for the connection only, which the client
/// aborted or reset before it got accepted.
fn connection_failed(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ECONNABORTED | libc::ECONNREFUSED | libc::ECONNRESET | libc::EPROTO)
    )
}

/// Create a non-blocking UNIX socket which only its owner can connect to.
/// Linux creates the file of a socket on bind with the mode of the socket
/// minus the umask, so others cannot connect at any time, unlike with a
/// chmod after the bind.
pub fn private_socket(typ: SockType) -> io::Result<OwnedFd> {
    let fd = socket(
        AddressFamily::Unix,
        typ,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    fchmod(&fd, Mode::S_IRWXU)?;
    Ok(fd)
}

/// Listen on a socket at `path` which only its owner can connect to, see
/// [`private_socket`].
pub fn listen_private(path: &Path, typ: SockType) -> io::Result<OwnedFd> {
    let fd = private_socket(typ)?;
    bind(fd.as_raw_fd(), &UnixAddr::new(path)?)?;
    listen(&fd, Backlog::MAXCONN)?;
    Ok(fd)
}

/// Bind a listener with `bind` at a socket path whatever its length, see
/// [`shorten_path`]. The parent directory gets created if it does not exist.
pub fn bind_long_path<L>(path: &Path, bind: impl FnOnce(PathBuf) -> io::Result<L>) -> Result<L> {
    // keep parent_fd in scope until the bind, or else the socket will not work
    let (path, _parent_dir) = shorten_socket_path(path)?;
    bind(path).context("bind server socket")
}

/// Like [`shorten_path`], creating the parent directory if it does not
/// exist.
pub fn shorten_socket_path(path: &Path) -> Result<(PathBuf, File)> {
    let (parent, _) = split_socket_path(path)?;
    fs::create_dir_all(parent).context("create parent directory")?;
    shorten_path(path)
}

/// Refer to a socket path through its parent directory, so that it fits into
/// a socket address whatever its length. The directory has to exist and
/// stays open as long as the returned file, the path is only valid until
/// then.
pub fn shorten_path(path: &Path) -> Result<(PathBuf, File)> {
    let (parent, name) = split_socket_path(path)?;
    let parent = File::open(parent).context("open parent directory")?;
    let fd = parent.as_raw_fd();

    Ok((
        PathBuf::from("/proc/self/fd")
            .join(fd.to_string())
            .join(name),
        parent,
    ))
}

/// Split a socket path into its parent directory and name, a relative name
/// lives in the current directory.
fn split_socket_path(path: &Path) -> Result<(&Path, &OsStr)> {
    let parent = path.parent().with_context(|| {
        format!(
            "tried to specify / as socket to bind to: {}",
            path.display()
        )
    })?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let name = path.file_name().with_context(|| {
        format!(
            "tried to specify '..' as socket to bind to: {}",
            path.display(),
        )
    })?;
    Ok((parent, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    #[test]
    fn accept_errors_get_logged_once_per_interval() {
        let mut errors = AcceptErrors::default();
        let start = Instant::now();
        assert!(errors.log_error_now(start));
        assert!(!errors.log_error_now(start + ACCEPT_BACKOFF));
        // Another listener logs its own.
        assert!(AcceptErrors::default().log_error_now(start + ACCEPT_BACKOFF));
        assert!(errors.log_error_now(start + ACCEPT_ERROR_INTERVAL));
    }

    #[tokio::test(start_paused = true)]
    async fn only_failed_connections_accept_at_once() {
        let start = time::Instant::now();
        let mut errors = AcceptErrors::default();
        for errno in [
            libc::ECONNABORTED,
            libc::ECONNREFUSED,
            libc::ECONNRESET,
            libc::EPROTO,
        ] {
            errors.failed(&io::Error::from_raw_os_error(errno)).await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
        errors
            .failed(&io::Error::from_raw_os_error(libc::EMFILE))
            .await;
        assert_eq!(start.elapsed(), ACCEPT_BACKOFF);
    }

    #[tokio::test]
    async fn bind_long_path_creates_the_directory() -> Result<()> {
        let dir = tempfile::tempdir()?;
        // Longer than a socket address holds.
        let parent = dir.path().join("x".repeat(100)).join("y".repeat(100));
        let path = parent.join("socket");
        let listener = bind_long_path(&path, UnixListener::bind)?;
        assert!(path.exists());
        drop(listener);

        // A path in use fails.
        assert!(bind_long_path(&path, UnixListener::bind).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn listen_private_binds_private_listeners() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("socket");
        let fd = bind_long_path(&path, |path| listen_private(&path, SockType::Stream))?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o077, 0);
        let listener = UnixListener::from_std(fd.into())?;
        let _client = tokio::net::UnixStream::connect(&path).await?;
        listener.accept().await?;
        Ok(())
    }

    #[test]
    fn shorten_socket_path_refers_to_the_parent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (path, parent) = shorten_socket_path(&dir.path().join("new").join("socket"))?;
        assert!(path.ends_with(format!("{}/socket", parent.as_raw_fd())));
        assert!(dir.path().join("new").is_dir());

        // Relative names live in the current directory.
        assert!(split_socket_path(Path::new("socket"))?.0 == Path::new("."));
        assert!(shorten_socket_path(Path::new("/")).is_err());
        Ok(())
    }

    #[test]
    fn shorten_socket_path_failures() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("file");
        fs::write(&file, "")?;
        // The parent cannot be created.
        assert!(shorten_socket_path(&file.join("socket")).is_err());
        // Without creating it, a missing parent cannot be opened.
        assert!(shorten_path(&dir.path().join("missing").join("socket")).is_err());
        Ok(())
    }

    #[test]
    fn private_socket_is_created_private() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("socket");
        let fd = private_socket(SockType::Stream)?;
        // The file gets the mode of the socket right on bind, before anything
        // could change it. The umask may take more bits, but never adds any.
        bind(fd.as_raw_fd(), &UnixAddr::new(&path)?)?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o077, 0);
        Ok(())
    }
}
