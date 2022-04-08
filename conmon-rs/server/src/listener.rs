use anyhow::{Context, Result};
use std::{
    fs::File,
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
};
use tokio::net::UnixListener;

pub fn bind_long_path(path: &Path) -> Result<UnixListener> {
    let parent = path.parent().context(format!(
        "tried to specify / as socket to bind to: {}",
        path.display()
    ))?;
    let name = path.file_name().context(format!(
        "tried to specify '..' as socket to bind to: {}",
        path.display(),
    ))?;

    let parent = File::open(parent)?;
    let fd = parent.as_raw_fd();
    let socket_path = PathBuf::from("/proc/self/fd")
        .join(fd.to_string())
        .join(name);

    UnixListener::bind(&socket_path).context("bind server socket")
}
