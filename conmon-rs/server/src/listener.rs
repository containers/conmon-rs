use anyhow::{Context, Result};
use std::{
    fs::{self, File},
    io,
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
};
use tokio::net::UnixListener;
use tokio_seqpacket::UnixSeqpacketListener;

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

        let parent = path.parent().with_context(|| {
            format!(
                "tried to specify / as socket to bind to: {}",
                path.display()
            )
        })?;
        let name = path.file_name().with_context(|| {
            format!(
                "tried to specify '..' as socket to bind to: {}",
                path.display(),
            )
        })?;

        self.imp
            .create_dir_all(parent)
            .context("create parent directory")?;

        let parent = self.imp.open(parent).context("open parent directory")?;
        let fd = parent.as_raw_fd();

        Ok((
            PathBuf::from("/proc/self/fd")
                .join(fd.to_string())
                .join(name),
            parent,
        ))
    }
}

#[cfg_attr(test, automock(type Listener = UnixListener;))]
pub trait ListenerImpl {
    type Listener;
    fn bind(&self, path: &Path) -> io::Result<Self::Listener>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn open(&self, path: &Path) -> io::Result<File>;
}

#[derive(Debug, Default)]
/// The default implementation for the Listener.
pub struct DefaultListener;

impl ListenerImpl for DefaultListener {
    type Listener = UnixListener;
    fn bind(&self, path: &Path) -> io::Result<Self::Listener> {
        UnixListener::bind(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }

    fn open(&self, path: &Path) -> io::Result<File> {
        File::open(path)
    }
}

#[derive(Debug, Default)]
/// The default implementation for the Listener.
pub struct SeqpacketListener;

impl ListenerImpl for SeqpacketListener {
    type Listener = UnixSeqpacketListener;
    fn bind(&self, path: &Path) -> io::Result<Self::Listener> {
        UnixSeqpacketListener::bind(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }

    fn open(&self, path: &Path) -> io::Result<File> {
        File::open(path)
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
