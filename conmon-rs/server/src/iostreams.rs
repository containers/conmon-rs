//! Pseudo terminal implementation.

use anyhow::{Context, Result};
use getset::{CopyGetters, Setters};
use log::{debug, error};
use nix::{
    fcntl::OFlag,
    sys::stat::{self, Mode},
    unistd,
};
use std::{
    fs::OpenOptions,
    io::{self, BufReader, Read, Write},
    os::unix::io::{IntoRawFd, RawFd},
    str, thread,
};

#[derive(Clone, Copy, Debug, CopyGetters, Setters)]
#[getset(get_copy, set)]
pub struct IOStreams {
    stdin: Option<Stream>,
    stderr: Stream,
    stdout: Stream,
}

#[derive(Clone, Copy, Debug)]
struct Stream(RawFd);

impl From<RawFd> for Stream {
    fn from(fd: RawFd) -> Self {
        Stream(fd)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unistd::read(self.0, buf).map_err(io::Error::from)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unistd::write(self.0, buf).map_err(io::Error::from)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl IOStreams {
    /// Create a new IOStreams instance.
    pub fn new() -> Result<Self> {
        debug!("Creating new IO streams");

        let (stdout_fd, _worker_stdout_fd) =
            unistd::pipe2(OFlag::O_CLOEXEC).context("create stdout pipe")?;

        let (stderr_fd, _worker_stderr_fd) =
            unistd::pipe2(OFlag::O_CLOEXEC).context("create stderr pipe")?;

        let mode = Mode::from_bits_truncate(0o777);
        stat::fchmod(libc::STDOUT_FILENO, mode).context("chmod stdout")?;
        stat::fchmod(libc::STDERR_FILENO, mode).context("chmod stderr")?;

        Ok(Self {
            stdin: None,
            stdout: stdout_fd.into(),
            stderr: stderr_fd.into(),
        })
    }

    /// Create a IOStreams from a single raw file descriptor.
    pub fn from_raw_fd(fd: RawFd) -> Result<Self> {
        debug!("Creating IO streams from raw file descriptor");
        const DEV_NULL: &str = "/dev/null";

        let worker_stdin_fd = OpenOptions::new().read(true).open(DEV_NULL)?.into_raw_fd();
        unistd::dup2(worker_stdin_fd, libc::STDIN_FILENO).context("dup over stdin")?;

        let worker_stdout_fd = OpenOptions::new().write(true).open(DEV_NULL)?.into_raw_fd();
        unistd::dup2(worker_stdout_fd, libc::STDOUT_FILENO).context("dup over stdout")?;

        let (stderr, _) = unistd::pipe2(OFlag::O_CLOEXEC).context("create stderr pipe")?;
        let worker_stderr_fd = worker_stdout_fd;
        unistd::dup2(worker_stderr_fd, libc::STDERR_FILENO).context("dup over stderr")?;

        Ok(Self {
            stdin: Some(fd.into()),
            stdout: fd.into(),
            stderr: stderr.into(),
        })
    }

    /// Start the internal reading and writing threads.
    pub fn start(&self) -> Result<()> {
        let stdin = self.stdin();
        let stdout = self.stdout();
        let stderr = self.stderr();
        thread::spawn(move || Self::read_loop(stdin, stdout, stderr));
        Ok(())
    }

    fn read_loop(stdin: Option<Stream>, _stdout: Stream, _stderr: Stream) -> Result<()> {
        debug!("Start reading from IO streams");

        if let Some(stdin_stream) = stdin {
            let mut reader = BufReader::new(stdin_stream);
            let mut buf = vec![0; 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        let slice = &buf[..n];
                        debug!("Read {} bytes: {:?}", n, str::from_utf8(slice)?);
                    }
                    Err(e) => error!("Unable to read from terminal: {}", e),
                    _ => {}
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempfile;

    #[test]
    fn new_success() -> Result<()> {
        let sut = IOStreams::new()?;
        assert!(sut.stdin.is_none());
        Ok(())
    }

    #[test]
    fn from_raw_fd_success_without_stdin() -> Result<()> {
        let fd = tempfile()?.into_raw_fd();
        let sut = IOStreams::from_raw_fd(fd)?;
        assert_eq!(sut.stdin.context("stdin not set")?.0, fd);
        assert!(sut.stderr.0 != fd);

        Ok(())
    }
}
