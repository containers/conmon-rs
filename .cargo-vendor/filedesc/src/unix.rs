#![deny(unsafe_op_in_unsafe_fn)]

use std::os::raw::c_int;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};

#[derive(Debug)]
/// Thin wrapper around an open file descriptor.
///
/// The wrapped file descriptor will be closed
/// when the wrapper is dropped.
pub struct FileDesc {
	fd: OwnedFd,
}

impl FileDesc {
	/// Create [`FileDesc`] from an owned file descriptor.
	///
	/// This does not do anything to the file descriptor other than wrap it.
	/// Notably, it does not set the `close-on-exec` flag.
	pub fn new(fd: OwnedFd) -> Self {
		Self { fd }
	}

	/// Wrap a raw file descriptor in a [`FileDesc`].
	///
	/// This does not do anything to the file descriptor other than wrapping it.
	/// Notably, it does not set the `close-on-exec` flag.
	///
	/// # Safety
	/// The input must be a valid file descriptor.
	/// The file descriptor must not be closed as long as it is managed by the created [`FileDesc`].
	pub unsafe fn from_raw_fd(fd: RawFd) -> Self {
		unsafe {
			Self::new(OwnedFd::from_raw_fd(fd))
		}
	}

	/// Duplicate a file descriptor from an object that has a file descriptor.
	///
	/// The new file descriptor will have the `close-on-exec` flag set.
	/// If the platform supports it, the flag will be set atomically.
	///
	/// The duplicated [`FileDesc`] will be the sole owner of the new file descriptor, but it will share ownership of the underlying kernel object.
	pub fn duplicate_from<T: AsFd>(other: T) -> std::io::Result<Self> {
		Ok(Self::new(other.as_fd().try_clone_to_owned()?))
	}

	/// Duplicate a raw file descriptor and wrap it in a [`FileDesc`].
	///
	/// The new file descriptor will have the `close-on-exec` flag set.
	/// If the platform supports it, the flag will be set atomically.
	///
	/// The duplicated [`FileDesc`] will be the sole owner of the new file descriptor, but it will share ownership of the underlying kernel object.
	///
	/// # Safety
	/// The file descriptor must be valid,
	/// and duplicating it must not violate the safety requirements of any object already using the file descriptor.
	pub unsafe fn duplicate_raw_fd(fd: RawFd) -> std::io::Result<Self> {
		unsafe {
			Self::duplicate_from(BorrowedFd::borrow_raw(fd))
		}
	}

	/// Get the file descriptor.
	///
	/// This function does not release ownership of the underlying file descriptor.
	/// The file descriptor will still be closed when the [`FileDesc`] is dropped.
	pub fn as_fd(&self) -> BorrowedFd {
		self.fd.as_fd()
	}

	/// Release and get the raw file descriptor.
	///
	/// This function releases ownership of the underlying file descriptor.
	/// The file descriptor will not be closed.
	pub fn into_fd(self) -> OwnedFd {
		self.fd
	}

	/// Get the raw file descriptor.
	///
	/// This function does not release ownership of the underlying file descriptor.
	/// The file descriptor will still be closed when the [`FileDesc`] is dropped.
	pub fn as_raw_fd(&self) -> RawFd {
		self.fd.as_raw_fd()
	}

	/// Release and get the raw file descriptor.
	///
	/// This function releases ownership of the underlying file descriptor.
	/// The file descriptor will not be closed.
	pub fn into_raw_fd(self) -> RawFd {
		self.fd.into_raw_fd()
	}

	/// Try to duplicate the file descriptor.
	///
	/// The duplicated [`FileDesc`] will be the sole owner of the new file descriptor, but it will share ownership of the underlying kernel object.
	///
	/// The new file descriptor will have the `close-on-exec` flag set.
	/// If the platform supports it, the flag will be set atomically.
	pub fn duplicate(&self) -> std::io::Result<Self> {
		Self::duplicate_from(self)
	}

	/// Change the close-on-exec flag of the file descriptor.
	///
	/// You should always try to create file descriptors with the close-on-exec flag already set atomically instead of changing it later with this function.
	/// Setting the flag later on introduces a race condition if another thread forks before the call to `set_close_on_exec` finishes.
	///
	/// You can use this without any race condition to disable the `close-on-exec` flag *after* forking but before executing a new program.
	pub fn set_close_on_exec(&self, close_on_exec: bool) -> std::io::Result<()> {
		unsafe {
			// TODO: Are there platforms where we need to preserve other bits?
			let arg = if close_on_exec { libc::FD_CLOEXEC } else { 0 };
			check_ret(libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFD, arg))?;
			Ok(())
		}
	}

	/// Check the close-on-exec flag of the file descriptor.
	pub fn get_close_on_exec(&self) -> std::io::Result<bool> {
		unsafe {
			let ret = check_ret(libc::fcntl(self.fd.as_raw_fd(), libc::F_GETFD, 0))?;
			Ok(ret & libc::FD_CLOEXEC != 0)
		}
	}
}

impl AsFd for FileDesc {
	fn as_fd(&self) -> BorrowedFd<'_> {
		self.fd.as_fd()
	}
}

impl From<OwnedFd> for FileDesc {
	fn from(value: OwnedFd) -> Self {
		Self::new(value)
	}
}

impl From<FileDesc> for OwnedFd {
	fn from(value: FileDesc) -> Self {
		value.fd
	}
}

impl FromRawFd for FileDesc {
	unsafe fn from_raw_fd(fd: RawFd) -> Self {
		unsafe {
			Self::from_raw_fd(fd)
		}
	}
}

impl AsRawFd for FileDesc {
	fn as_raw_fd(&self) -> RawFd {
		self.as_raw_fd()
	}
}

impl AsRawFd for &'_ FileDesc {
	fn as_raw_fd(&self) -> RawFd {
		(*self).as_raw_fd()
	}
}

impl IntoRawFd for FileDesc {
	fn into_raw_fd(self) -> RawFd {
		self.into_raw_fd()
	}
}

/// Wrap the return value of a libc function in an [`std::io::Result`].
///
/// If the return value is -1, [`last_os_error()`](std::io::Error::last_os_error) is returned.
/// Otherwise, the return value is returned wrapped as [`Ok`].
fn check_ret(ret: c_int) -> std::io::Result<c_int> {
	if ret == -1 {
		Err(std::io::Error::last_os_error())
	} else {
		Ok(ret)
	}
}
