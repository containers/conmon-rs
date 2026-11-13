//! Waiting for descriptors which tokio has no type of its own for, like
//! pidfds, terminals, epoll and inotify instances.

use std::{
    io,
    ops::Deref,
    os::fd::{AsFd, AsRawFd, RawFd},
};
use tokio::io::{Interest, unix::AsyncFd};

/// A value holding a descriptor, which tokio waits for while it is registered,
/// see [`register`].
#[derive(Debug)]
pub(crate) struct Fd<T> {
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
///
/// The [`Fd`] has to stay in its [`AsyncFd`]: the mutable accessors of it,
/// like [`AsyncFd::get_mut`], must not be used to swap it with the one of
/// another registration, see the safety comment below.
pub(crate) fn register<T: AsFd>(inner: T, interest: Interest) -> io::Result<AsyncFd<Fd<T>>> {
    let raw = inner.as_fd().as_raw_fd();
    #[expect(unsafe_code, reason = "registering a descriptor with tokio")]
    // SAFETY: the descriptor is the one taken here, which `Fd` returns from
    // then on. It stays open and refers to the same file for as long as the
    // value lives: I/O safety does not let shared access close or replace
    // the descriptor of an `AsFd`, and `Fd` hands out nothing else of the
    // value. The `AsyncFd` owns the `Fd`, but nothing enforces that it keeps
    // it: its mutable accessors would let safe code swap two of them, each
    // registration would have the descriptor of the other one then. No
    // caller uses them, and none may.
    let fd = unsafe { AsyncFd::register_with_interest(Fd { inner, raw }, interest) }?;
    Ok(fd)
}
