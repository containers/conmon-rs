use nix::unistd;
use std::{
    io::{self, Read, Write},
    os::unix::io::RawFd,
};

#[derive(Clone, Copy, Debug)]
/// Stream is a IO abstraction over a raw file descriptor.
pub struct Stream(RawFd);

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
