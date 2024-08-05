use filedesc::FileDesc;
use std::os::raw::c_int;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::task::{Context, Poll};
use tokio::io::unix::AsyncFd;

use crate::{sys, UnixSeqpacket};

/// Listener for Unix seqpacket sockets.
pub struct UnixSeqpacketListener {
	io: AsyncFd<FileDesc>,
}

impl std::fmt::Debug for UnixSeqpacketListener {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		f.debug_struct("UnixSeqpacketListener")
			.field("fd", &self.io.get_ref().as_raw_fd())
			.finish()
	}
}

impl AsFd for UnixSeqpacketListener {
	fn as_fd(&self) -> BorrowedFd<'_> {
		self.io.get_ref().as_fd()
	}
}

impl TryFrom<OwnedFd> for UnixSeqpacketListener {
	type Error = std::io::Error;

	fn try_from(fd: OwnedFd) -> Result<Self, Self::Error> {
		Self::new(FileDesc::new(fd))
	}
}

impl From<UnixSeqpacketListener> for OwnedFd {
	fn from(socket: UnixSeqpacketListener) -> Self {
		socket.io.into_inner().into_fd()
	}
}

impl UnixSeqpacketListener {
	fn new(socket: FileDesc) -> std::io::Result<Self> {
		let io = AsyncFd::new(socket)?;
		Ok(Self { io })
	}

	/// Bind a new seqpacket listener to the given address.
	///
	/// The create listener will be ready to accept new connections.
	pub fn bind<P: AsRef<Path>>(address: P) -> std::io::Result<Self> {
		Self::bind_with_backlog(address, 128)
	}

	/// Bind a new seqpacket listener to the given address.
	///
	/// The create listener will be ready to accept new connections.
	///
	/// The `backlog` parameter is used to determine the size of connection queue.
	/// See `man 3 listen` for more information.
	pub fn bind_with_backlog<P: AsRef<Path>>(address: P, backlog: c_int) -> std::io::Result<Self> {
		let socket = sys::local_seqpacket_socket()?;
		sys::bind(&socket, address)?;
		sys::listen(&socket, backlog)?;
		Self::new(socket)
	}

	/// Wrap a raw file descriptor as [`UnixSeqpacket`].
	///
	/// Registration of the file descriptor with the tokio runtime may fail.
	/// For that reason, this function returns a [`std::io::Result`].
	///
	/// # Safety
	/// This function is unsafe because the socket assumes it is the sole owner of the file descriptor.
	/// Usage of this function could accidentally allow violating this contract
	/// which can cause memory unsafety in code that relies on it being true.
	pub unsafe fn from_raw_fd(fd: std::os::unix::io::RawFd) -> std::io::Result<Self> {
		Self::new(FileDesc::from_raw_fd(fd))
	}

	/// Get the raw file descriptor of the socket.
	pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
		self.io.as_raw_fd()
	}

	/// Deregister the socket from the tokio runtime and return the inner file descriptor.
	pub fn into_raw_fd(self) -> std::os::unix::io::RawFd {
		self.io.into_inner().into_raw_fd()
	}

	/// Get the socket address of the local half of this connection.
	pub fn local_addr(&self) -> std::io::Result<PathBuf> {
		sys::get_local_address(self.io.get_ref())
	}

	/// Get and clear the value of the `SO_ERROR` option.
	pub fn take_error(&self) -> std::io::Result<Option<std::io::Error>> {
		sys::take_socket_error(self.io.get_ref())
	}

	/// Check if there is a connection ready to accept.
	///
	/// Note that unlike [`Self::accept`], only the last task calling this function will be woken up.
	/// For that reason, it is preferable to use the async functions rather than polling functions when possible.
	///
	/// Note that this function does not return a remote address for the accepted connection.
	/// This is because connected Unix sockets are anonymous and have no meaningful address.
	pub fn poll_accept(&mut self, cx: &mut Context) -> Poll<std::io::Result<UnixSeqpacket>> {
		let socket = loop {
			let mut ready_guard = ready!(self.io.poll_read_ready(cx)?);

			match ready_guard.try_io(|inner| sys::accept(inner.get_ref())) {
				Ok(x) => break x?,
				Err(_would_block) => continue,
			}
		};

		Poll::Ready(Ok(UnixSeqpacket::new(socket)?))
	}

	/// Accept a new incoming connection on the listener.
	///
	/// This function is safe to call concurrently from different tasks.
	/// Although no order is guaranteed, all calling tasks will try to complete the asynchronous action.
	///
	/// Note that this function does not return a remote address for the accepted connection.
	/// This is because connected Unix sockets are anonymous and have no meaningful address.
	pub async fn accept(&mut self) -> std::io::Result<UnixSeqpacket> {
		let socket = loop {
			let mut ready_guard = self.io.readable().await?;

			match ready_guard.try_io(|inner| sys::accept(inner.get_ref())) {
				Ok(x) => break x?,
				Err(_would_block) => continue,
			}
		};

		UnixSeqpacket::new(socket)
	}
}

impl AsRawFd for UnixSeqpacketListener {
	fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
		self.as_raw_fd()
	}
}

impl IntoRawFd for UnixSeqpacketListener {
	fn into_raw_fd(self) -> std::os::unix::io::RawFd {
		self.into_raw_fd()
	}
}
