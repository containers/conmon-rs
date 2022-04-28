use anyhow::{Context, Result};
use std::{
    fs,
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
};
use tokio::net::UnixListener;

pub fn bind_long_path(path: &Path) -> Result<UnixListener> {
    // keep parent_fd in scope until the bind, or else the socket will not work
    let (path, _parent_dir) = shorten_socket_path(path)?;
    UnixListener::bind(&path).context("bind server socket")
}

pub fn shorten_socket_path(path: &Path) -> Result<(PathBuf, fs::File)> {
    let parent = path.parent().context(format!(
        "tried to specify / as socket to bind to: {}",
        path.display()
    ))?;
    let name = path.file_name().context(format!(
        "tried to specify '..' as socket to bind to: {}",
        path.display(),
    ))?;

    fs::create_dir_all(parent).context("create parent directory")?;
    let parent = fs::File::open(parent).context("open parent directory")?;
    let fd = parent.as_raw_fd();
    Ok((
        PathBuf::from("/proc/self/fd")
            .join(fd.to_string())
            .join(name),
        parent,
    ))
}
