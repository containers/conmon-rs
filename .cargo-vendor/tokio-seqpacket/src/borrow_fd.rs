//! Utility module for borrowing file descriptors with a specific lifetime.

use std::os::fd::{BorrowedFd, AsFd};

/// Trait for types that can give a [`BorrowedFd<'a>`].
///
/// This is automatically implemented for all `&'a T` where `T: AsFd`,
/// and for `BorrowedFd<'a>`.
pub trait BorrowFd<'a> {
	/// Borrow the file descriptor.
	fn borrow_fd(&self) -> BorrowedFd<'a>;
}

impl<'a, T: AsFd> BorrowFd<'a> for &'a T {
	fn borrow_fd(&self) -> BorrowedFd<'a> {
		(*self).as_fd()
	}
}

impl<'a> BorrowFd<'a> for BorrowedFd<'a> {
	fn borrow_fd(&self) -> BorrowedFd<'a> {
		*self
	}
}
