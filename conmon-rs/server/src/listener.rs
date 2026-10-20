use anyhow::{Context, Result};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{net::UnixListener, time};
use tokio_seqpacket::UnixSeqpacketListener;
use tracing::error;

/// How long an accept loop pauses after a failed accept.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Handle a failed accept of a listener which keeps going. Most errors, like
/// running out of descriptors, fail every accept until something changes, so
/// pause instead of spinning. Only a connection which failed on its own is
/// no reason to wait.
pub async fn accept_failed(e: &io::Error) {
    error!("Unable to accept connection: {e}");
    if !matches!(e.raw_os_error(), Some(libc::ECONNABORTED | libc::EPROTO)) {
        time::sleep(ACCEPT_BACKOFF).await;
    }
}

#[cfg(test)]
use mockall::{automock, predicate::*};

#[derive(Debug, Default)]
/// The main structure for this module.
pub struct Listener<T> {
    imp: T,
}

impl<T> Listener<T>
where
    T: ListenerImpl,
{
    pub fn bind_long_path<P>(&self, path: P) -> Result<T::Listener>
    where
        P: AsRef<Path>,
    {
        // keep parent_fd in scope until the bind, or else the socket will not work
        let (path, _parent_dir) = self.shorten_socket_path(path)?;
        self.imp.bind(path.as_ref()).context("bind server socket")
    }

    pub fn shorten_socket_path<P>(&self, path: P) -> Result<(PathBuf, File)>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let (parent, _) = split_socket_path(path)?;

        self.imp
            .create_dir_all(parent)
            .context("create parent directory")?;

        shorten_path_with(path, |parent| self.imp.open(parent))
    }
}

/// Refer to a socket path through its parent directory, so that it fits into
/// a socket address whatever its length. The directory has to exist and
/// stays open as long as the returned file, the path is only valid until
/// then.
pub fn shorten_path(path: &Path) -> Result<(PathBuf, File)> {
    shorten_path_with(path, |parent| File::open(parent))
}

fn shorten_path_with<F>(path: &Path, open: F) -> Result<(PathBuf, File)>
where
    F: FnOnce(&Path) -> io::Result<File>,
{
    let (parent, name) = split_socket_path(path)?;
    let parent = open(parent).context("open parent directory")?;
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

#[cfg_attr(test, automock(type Listener = UnixListener;))]
pub trait ListenerImpl {
    type Listener;
    fn bind(&self, path: &Path) -> io::Result<Self::Listener>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }
    fn open(&self, path: &Path) -> io::Result<File> {
        File::open(path)
    }
}

#[derive(Debug, Default)]
/// The default implementation for the Listener.
pub struct DefaultListener;

impl ListenerImpl for DefaultListener {
    type Listener = UnixListener;
    fn bind(&self, path: &Path) -> io::Result<Self::Listener> {
        UnixListener::bind(path)
    }
}

#[derive(Debug, Default)]
/// The implementation for SOCK_SEQPACKET listeners.
pub struct SeqpacketListener;

impl ListenerImpl for SeqpacketListener {
    type Listener = UnixSeqpacketListener;
    fn bind(&self, path: &Path) -> io::Result<Self::Listener> {
        UnixSeqpacketListener::bind(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use tempfile::{tempdir, tempfile};

    fn new_sut(mock: MockListenerImpl) -> Listener<MockListenerImpl> {
        Listener::<MockListenerImpl> { imp: mock }
    }

    fn permission_denied<T>() -> Result<T, io::Error> {
        Err(io::Error::new(ErrorKind::PermissionDenied, ""))
    }

    #[tokio::test]
    async fn bind_long_path_success() -> Result<()> {
        let mut mock = MockListenerImpl::new();

        mock.expect_create_dir_all().returning(|_| Ok(()));
        mock.expect_open().returning(|_| tempfile());
        mock.expect_bind()
            .returning(|_| UnixListener::bind(tempdir()?.path().join("foo")));

        let sut = new_sut(mock);
        let first = "foo";
        let listener = sut.bind_long_path(PathBuf::from(first).join("bar"))?;

        let addr = listener.local_addr()?;
        assert!(addr.as_pathname().context("no path name")?.ends_with(first));

        Ok(())
    }

    #[tokio::test]
    async fn bind_long_path_failure_on_bind() {
        let mut mock = MockListenerImpl::new();

        mock.expect_create_dir_all().returning(|_| Ok(()));
        mock.expect_open().returning(|_| tempfile());
        mock.expect_bind().returning(|_| permission_denied());

        let sut = new_sut(mock);
        assert!(
            sut.bind_long_path(PathBuf::from("foo").join("bar"))
                .is_err()
        );
    }

    #[test]
    fn shorten_socket_path_success() -> Result<()> {
        let mut mock = MockListenerImpl::new();

        mock.expect_create_dir_all().returning(|_| Ok(()));
        mock.expect_open().returning(|_| tempfile());

        let sut = new_sut(mock);
        let last = "bar";
        let (res_file_path, res_parent) =
            sut.shorten_socket_path(PathBuf::from("/foo").join(last))?;

        assert!(res_file_path.ends_with(last));
        assert!(
            res_file_path
                .display()
                .to_string()
                .contains(&res_parent.as_raw_fd().to_string())
        );

        Ok(())
    }

    #[test]
    fn shorten_socket_path_failure_on_open() {
        let mut mock = MockListenerImpl::new();

        mock.expect_create_dir_all().returning(|_| Ok(()));
        mock.expect_open().returning(|_| permission_denied());

        let sut = new_sut(mock);

        assert!(sut.shorten_socket_path("/foo/bar").is_err());
    }

    #[test]
    fn shorten_socket_path_failure_on_create_dir_all() {
        let mut mock = MockListenerImpl::new();

        mock.expect_create_dir_all()
            .returning(|_| permission_denied());

        let sut = new_sut(mock);

        assert!(sut.shorten_socket_path("/foo/bar").is_err());
    }
}
