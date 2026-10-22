//! Waiting for descriptors which tokio has no type of its own for, like
//! pidfds, terminals, epoll, inotify and eventfd instances.

use std::{
    io,
    ops::Deref,
    os::fd::{AsFd, AsRawFd, RawFd},
};
use tokio::io::{Interest, unix::AsyncFd};

/// A value holding a descriptor, which tokio waits for while it is registered,
/// see [`register`].
#[derive(Debug)]
pub struct Fd<T> {
    inner: T,
    raw: RawFd,
}

impl<T> Deref for Fd<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> AsRawFd for Fd<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.raw
    }
}

/// Register the descriptor of a value with the reactor of the runtime, the
/// returned [`AsyncFd`] owns the value.
pub fn register<T: AsFd>(inner: T, interest: Interest) -> io::Result<AsyncFd<Fd<T>>> {
    let raw = inner.as_fd().as_raw_fd();
    #[expect(unsafe_code, reason = "registering a descriptor with tokio")]
    // SAFETY: the descriptor stays open and refers to the same file for as
    // long as the value lives, which the `AsyncFd` owns: I/O safety does not
    // let anything else close or replace the descriptor of an `AsFd` while
    // it lives, and only shared access to the value gets handed out. The
    // descriptor is the one taken here.
    let fd = unsafe { AsyncFd::register_with_interest(Fd { inner, raw }, interest) }?;
    Ok(fd)
}
