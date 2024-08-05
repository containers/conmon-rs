use filedesc::FileDesc;
use std::io::{IoSlice, IoSliceMut};
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd};
use std::path::Path;
use std::task::{Context, Poll};
use tokio::io::unix::AsyncFd;

use crate::ancillary::{AncillaryMessageReader, AncillaryMessageWriter};
use crate::{sys, UCred};

/// Unix seqpacket socket.
///
/// Note that there are no functions to get the local or remote address of the connection.
/// That is because connected Unix sockets are always anonymous,
/// which means that the address contains no useful information.
pub struct UnixSeqpacket {
	io: AsyncFd<FileDesc>,
}

impl std::fmt::Debug for UnixSeqpacket {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		f.debug_struct("UnixSeqpacket")
			.field("fd", &self.io.get_ref().as_raw_fd())
			.finish()
	}
}

impl AsFd for UnixSeqpacket {
	fn as_fd(&self) -> BorrowedFd<'_> {
		self.io.get_ref().as_fd()
	}
}

impl TryFrom<OwnedFd> for UnixSeqpacket {
	type Error = std::io::Error;

	fn try_from(fd: OwnedFd) -> Result<Self, Self::Error> {
		Self::new(FileDesc::new(fd))
	}
}

impl From<UnixSeqpacket> for OwnedFd {
	fn from(socket: UnixSeqpacket) -> Self {
		socket.io.into_inner().into_fd()
	}
}

impl UnixSeqpacket {
	pub(crate) fn new(socket: FileDesc) -> std::io::Result<Self> {
		let io = AsyncFd::new(socket)?;
		Ok(Self { io })
	}

	/// Connect a new seqpacket socket to the given address.
	pub async fn connect<P: AsRef<Path>>(address: P) -> std::io::Result<Self> {
		let socket = sys::local_seqpacket_socket()?;
		if let Err(e) = sys::connect(&socket, address) {
			if e.kind() != std::io::ErrorKind::WouldBlock {
				return Err(e);
			}
		}

		let socket = Self::new(socket)?;
		socket.io.writable().await?.retain_ready();
		Ok(socket)
	}

	/// Create a pair of connected seqpacket sockets.
	pub fn pair() -> std::io::Result<(Self, Self)> {
		let (a, b) = sys::local_seqpacket_pair()?;
		Ok((Self::new(a)?, Self::new(b)?))
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
	///
	/// This is a shortcut for `seqpacket.as_async_fd().as_raw_fd()`.
	/// See [`as_async_fd`](Self::as_async_fd).
	pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
		self.io.as_raw_fd()
	}

	/// Deregister the socket from the tokio runtime and return the inner file descriptor.
	pub fn into_raw_fd(self) -> std::os::unix::io::RawFd {
		self.io.into_inner().into_raw_fd()
	}

	#[doc(hidden)]
	#[deprecated(
		since = "0.4.0",
		note = "all I/O functions now take a shared reference to self, so splitting is no longer necessary"
	)]
	pub fn split(&self) -> (&Self, &Self) {
		(self, self)
	}

	/// Get the async file descriptor of this object.
	///
	/// This can be useful for applications that want to do low-level socket calls, such as
	/// [`sendmsg`](libc::sendmsg), but still want to use async and need to know when the socket is
	/// ready to be used.
	///
	/// Example:
	/// ```
	/// # async fn f() -> std::io::Result<()> {
	/// let seqpacket = tokio_seqpacket::UnixSeqpacket::connect("/tmp/example.sock").await?;
	/// seqpacket.as_async_fd().writable().await?.retain_ready();
	/// # Ok(()) }
	/// ```
	pub fn as_async_fd(&self) -> &AsyncFd<FileDesc> {
		&self.io
	}

	/// Get the effective credentials of the process which called `connect` or `pair`.
	///
	/// Note that this is not necessarily the process that currently has the file descriptor
	/// of the other side of the connection.
	pub fn peer_cred(&self) -> std::io::Result<UCred> {
		UCred::from_socket_peer(&self.io)
	}

	/// Get and clear the value of the `SO_ERROR` option.
	pub fn take_error(&self) -> std::io::Result<Option<std::io::Error>> {
		sys::take_socket_error(self.io.get_ref())
	}

	/// Try to send data on the socket to the connected peer without blocking.
	///
	/// If the socket is not ready yet, the current task is scheduled to wake up when the socket becomes writeable.
	///
	/// Note that unlike [`Self::send`], only the last task calling this function will be woken up.
	/// For that reason, it is preferable to use the async functions rather than polling functions when possible.
	pub fn poll_send(&self, cx: &mut Context, buffer: &[u8]) -> Poll<std::io::Result<usize>> {
		loop {
			let mut ready_guard = ready!(self.io.poll_write_ready(cx)?);

			match ready_guard.try_io(|inner| sys::send(inner.get_ref(), buffer)) {
				Ok(result) => return Poll::Ready(result),
				Err(_would_block) => continue,
			}
		}
	}

	/// Try to send data on the socket to the connected peer without blocking.
	///
	/// If the socket is not ready yet, the current task is scheduled to wake up when the socket becomes writeable.
	///
	/// Note that unlike [`Self::send_vectored`], only the last task calling this function will be woken up.
	/// For that reason, it is preferable to use the async functions rather than polling functions when possible.
	pub fn poll_send_vectored(&self, cx: &mut Context, buffer: &[IoSlice]) -> Poll<std::io::Result<usize>> {
		self.poll_send_vectored_with_ancillary(cx, buffer, &mut AncillaryMessageWriter::new(&mut []))
	}

	/// Try to send data with ancillary data on the socket to the connected peer without blocking.
	///
	/// If the socket is not ready yet, the current task is scheduled to wake up when the socket becomes writeable.
	///
	/// Note that unlike [`Self::send_vectored_with_ancillary`], only the last task calling this function will be woken up.
	/// For that reason, it is preferable to use the async functions rather than polling functions when possible.
	pub fn poll_send_vectored_with_ancillary(
		&self,
		cx: &mut Context,
		buffer: &[IoSlice],
		ancillary: &mut AncillaryMessageWriter,
	) -> Poll<std::io::Result<usize>> {
		loop {
			let mut ready_guard = ready!(self.io.poll_write_ready(cx)?);
			match ready_guard.try_io(|inner| sys::send_msg(inner.get_ref(), buffer, ancillary)) {
				Ok(result) => return Poll::Ready(result),
				Err(_would_block) => continue,
			}
		}
	}

	/// Send data on the socket to the connected peer.
	///
	/// This function is safe to call concurrently from different tasks.
	/// All calling tasks will try to complete the asynchronous action,
	/// although the order in which they complete is not guaranteed.
	pub async fn send(&self, buffer: &[u8]) -> std::io::Result<usize> {
		loop {
			let mut ready_guard = self.io.writable().await?;

			match ready_guard.try_io(|inner| sys::send(inner.get_ref(), buffer)) {
				Ok(result) => return result,
				Err(_would_block) => continue,
			}
		}
	}

	/// Send data on the socket to the connected peer.
	///
	/// This function is safe to call concurrently from different tasks.
	/// All calling tasks will try to complete the asynchronous action,
	/// although the order in which they complete is not guaranteed.
	pub async fn send_vectored(&self, buffer: &[IoSlice<'_>]) -> std::io::Result<usize> {
		self.send_vectored_with_ancillary(buffer, &mut AncillaryMessageWriter::new(&mut []))
			.await
	}

	/// Send data with ancillary data on the socket to the connected peer.
	///
	/// This function is safe to call concurrently from different tasks.
	/// All calling tasks will try to complete the asynchronous action,
	/// although the order in which they complete is not guaranteed.
	pub async fn send_vectored_with_ancillary(
		&self,
		buffer: &[IoSlice<'_>],
		ancillary: &mut AncillaryMessageWriter<'_>,
	) -> std::io::Result<usize> {
		loop {
			let mut ready_guard = self.io.writable().await?;
			match ready_guard.try_io(|inner| sys::send_msg(inner.get_ref(), buffer, ancillary)) {
				Ok(result) => return result,
				Err(_would_block) => continue,
			}
		}
	}

	/// Try to receive data on the socket from the connected peer without blocking.
	///
	/// If there is no data ready yet, the current task is scheduled to wake up when the socket becomes readable.
	///
	/// Note that unlike [`Self::recv`], only the last task calling this function will be woken up.
	/// For that reason, it is preferable to use the async functions rather than polling functions when possible.
	pub fn poll_recv(&self, cx: &mut Context, buffer: &mut [u8]) -> Poll<std::io::Result<usize>> {
		loop {
			let mut ready_guard = ready!(self.io.poll_read_ready(cx)?);
			match ready_guard.try_io(|inner| sys::recv(inner.get_ref(), buffer)) {
				Ok(result) => return Poll::Ready(result),
				Err(_would_block) => continue,
			}
		}
	}

	/// Try to receive data on the socket from the connected peer without blocking.
	///
	/// If there is no data ready yet, the current task is scheduled to wake up when the socket becomes readable.
	///
	/// Note that unlike [`Self::recv_vectored`], only the last task calling this function will be woken up.
	/// For that reason, it is preferable to use the async functions rather than polling functions when possible.
	pub fn poll_recv_vectored(&self, cx: &mut Context, buffer: &mut [IoSliceMut]) -> Poll<std::io::Result<usize>> {
		let (read, _ancillary) = ready!(self.poll_recv_vectored_with_ancillary(cx, buffer, &mut []))?;
		Poll::Ready(Ok(read))
	}

	/// Try to receive data with ancillary data on the socket from the connected peer without blocking.
	///
	/// Any file descriptors received in the anicallary data will have the `close-on-exec` flag set.
	/// If the OS supports it, this is done atomically with the reception of the message.
	/// However, on Illumos and Solaris, the `close-on-exec` flag is set in a separate step after receiving the message.
	///
	/// Note that you should always wrap or close any file descriptors received this way.
	/// If you do not, the received file descriptors will stay open until the process is terminated.
	///
	/// If there is no data ready yet, the current task is scheduled to wake up when the socket becomes readable.
	///
	/// Note that unlike [`Self::recv_vectored_with_ancillary`], only the last task calling this function will be woken up.
	/// For that reason, it is preferable to use the async functions rather than polling functions when possible.
	pub fn poll_recv_vectored_with_ancillary<'a>(
		&self,
		cx: &mut Context,
		buffer: &mut [IoSliceMut],
		ancillary_buffer: &'a mut [u8],
	) -> Poll<std::io::Result<(usize, AncillaryMessageReader<'a>)>> {
		loop {
			let mut ready_guard = ready!(self.io.poll_read_ready(cx)?);

			let (read, ancillary_reader) = match ready_guard.try_io(|inner| sys::recv_msg(inner.get_ref(), buffer, ancillary_buffer)) {
				Ok(x) => x?,
				Err(_would_block) => continue,
			};

			// SAFETY: We have to work around a borrow checker bug:
			// It doesn't know that we return in this branch, so the loop terminates.
			// It thinks we will do another mutable borrow in the next loop iteration.
			// TODO: Remove this transmute once the borrow checker is smart enough.
			return Poll::Ready(Ok((read, unsafe { transmute_lifetime(ancillary_reader) })));
		}
	}

	/// Receive data on the socket from the connected peer.
	///
	/// This function is safe to call concurrently from different tasks.
	/// All calling tasks will try to complete the asynchronous action,
	/// although the order in which they complete is not guaranteed.
	pub async fn recv(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
		loop {
			let mut ready_guard = self.io.readable().await?;
			match ready_guard.try_io(|inner| sys::recv(inner.get_ref(), buffer)) {
				Ok(result) => return result,
				Err(_would_block) => continue,
			}
		}
	}

	/// Receive data on the socket from the connected peer.
	///
	/// This function is safe to call concurrently from different tasks.
	/// All calling tasks will try to complete the asynchronous action,
	/// although the order in which they complete is not guaranteed.
	pub async fn recv_vectored(&self, buffer: &mut [IoSliceMut<'_>]) -> std::io::Result<usize> {
		let (read, _ancillary) = self.recv_vectored_with_ancillary(buffer, &mut [])
			.await?;
		Ok(read)
	}

	/// Receive data with ancillary data on the socket from the connected peer.
	///
	/// Any file descriptors received in the anicallary data will have the `close-on-exec` flag set.
	/// If the OS supports it, this is done atomically with the reception of the message.
	/// However, on Illumos and Solaris, the `close-on-exec` flag is set in a separate step after receiving the message.
	///
	/// Note that you should always wrap or close any file descriptors received this way.
	/// If you do not, the received file descriptors will stay open until the process is terminated.
	///
	/// This function is safe to call concurrently from different tasks.
	/// All calling tasks will try to complete the asynchronous action,
	/// although the order in which they complete is not guaranteed.
	pub async fn recv_vectored_with_ancillary<'a>(
		&self,
		buffer: &mut [IoSliceMut<'_>],
		ancillary_buffer: &'a mut [u8],
	) -> std::io::Result<(usize, AncillaryMessageReader<'a>)> {
		loop {
			let mut ready_guard = self.io.readable().await?;

			let (read, ancillary_reader) = match ready_guard.try_io(|inner| sys::recv_msg(inner.get_ref(), buffer, ancillary_buffer)) {
				Ok(x) => x?,
				Err(_would_block) => continue,
			};

			// SAFETY: We have to work around a borrow checker bug:
			// It doesn't know that we return in this branch, so the loop terminates.
			// It thinks we will do another mutable borrow in the next loop iteration.
			// TODO: Remove this transmute once the borrow checker is smart enough.
			return Ok((read, unsafe { transmute_lifetime(ancillary_reader) }));
		}
	}

	/// Shuts down the read, write, or both halves of this connection.
	///
	/// This function will cause all pending and future I/O calls on the
	/// specified portions to immediately return with an appropriate value
	/// (see the documentation of `Shutdown`).
	pub fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
		sys::shutdown(self.io.get_ref(), how)
	}
}

impl AsRawFd for UnixSeqpacket {
	fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
		self.as_raw_fd()
	}
}

impl IntoRawFd for UnixSeqpacket {
	fn into_raw_fd(self) -> std::os::unix::io::RawFd {
		self.into_raw_fd()
	}
}

/// Transmute the lifetime of a `AncillaryMessageReader`.
///
/// Exists to ensure we do not accidentally transmute more than we intend to.
///
/// # Safety
/// All the safety requirements of [`std::mem::transmute`] should be uphold.
#[allow(clippy::needless_lifetimes)]
unsafe fn transmute_lifetime<'a, 'b>(input: AncillaryMessageReader<'a>) -> AncillaryMessageReader<'b> {
	std::mem::transmute(input)
}
