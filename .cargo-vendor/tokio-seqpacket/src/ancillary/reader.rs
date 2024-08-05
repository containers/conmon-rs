use std::os::fd::{OwnedFd, BorrowedFd};

use super::FD_SIZE;

/// Reader to parse received ancillary messages from a Unix socket.
///
/// # Example
/// ```no_run
/// use tokio_seqpacket::UnixSeqpacket;
/// use tokio_seqpacket::ancillary::{AncillaryMessageReader, AncillaryMessage};
/// use std::io::IoSliceMut;
/// use std::os::fd::AsRawFd;
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let sock = UnixSeqpacket::connect("/tmp/sock").await?;
///
///     let mut fds = [0; 8];
///     let mut ancillary_buffer = [0; 128];
///
///     let mut buf = [1; 8];
///     let mut bufs = [IoSliceMut::new(&mut buf)];
///     let (_read, ancillary) = sock.recv_vectored_with_ancillary(&mut bufs, &mut ancillary_buffer).await?;
///
///     for message in ancillary.messages() {
///         if let AncillaryMessage::FileDescriptors(fds) = message {
///             for fd in fds {
///                 println!("received file descriptor: {}", fd.as_raw_fd());
///             }
///         }
///     }
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct AncillaryMessageReader<'a> {
	pub(crate) buffer: &'a mut [u8],
	pub(crate) truncated: bool,
}

/// Iterator over ancillary messages from a [`AncillaryMessageReader`].
#[derive(Copy, Clone)]
pub struct AncillaryMessages<'a> {
	buffer: &'a [u8],
	current: Option<&'a libc::cmsghdr>,
}

/// Owning iterator over ancillary messages from a [`AncillaryMessageReader`].
pub struct IntoAncillaryMessages<'a> {
	buffer: &'a mut [u8],
	current: Option<&'a libc::cmsghdr>,
}

/// This enum represent one control message of variable type.
pub enum AncillaryMessage<'a> {
	/// Ancillary message holding file descriptors.
	FileDescriptors(FileDescriptors<'a>),

	/// Ancillary message holding unix credentials.
	#[cfg(any(doc, target_os = "android", target_os = "linux", target_os = "netbsd",))]
	Credentials(UnixCredentials<'a>),

	/// Ancillary message uninterpreted data.
	Other(UnknownMessage<'a>)
}

/// This enum represent one control message of variable type.
///
/// Where applicable, it has taken ownership of the objects in the control message.
pub enum OwnedAncillaryMessage<'a> {
	/// Ancillary message holding file descriptors.
	FileDescriptors(OwnedFileDescriptors<'a>),

	/// Ancillary message holding unix credentials.
	#[cfg(any(doc, target_os = "android", target_os = "linux", target_os = "netbsd",))]
	Credentials(UnixCredentials<'a>),

	/// Ancillary message uninterpreted data.
	Other(UnknownMessage<'a>)
}

/// A control message containing borrowed file descriptors.
#[derive(Copy, Clone)]
pub struct FileDescriptors<'a> {
	/// The message data.
	data: &'a [u8],
}

/// A control message containing owned file descriptors.
pub struct OwnedFileDescriptors<'a> {
	/// The message data.
	data: &'a mut [u8],
}

/// A control message containing unix credentials for a process.
#[derive(Copy, Clone)]
#[cfg(any(doc, target_os = "linux", target_os = "android", target_os = "netbsd"))]
pub struct UnixCredentials<'a> {
	/// The message data.
	data: &'a [u8],
}

/// An unrecognized control message.
#[derive(Copy, Clone)]
pub struct UnknownMessage<'a> {
	/// The `cmsg_level` field of the ancillary data.
	cmsg_level: i32,

	/// The `cmsg_type` field of the ancillary data.
	cmsg_type: i32,

	/// The message data.
	data: &'a [u8],
}

impl<'a> AncillaryMessageReader<'a> {
	/// Create an ancillary data with the given buffer.
	///
	/// # Safety
	/// The memory buffer must contain valid ancillary messages received from the kernel for a Unix socket.
	///
	/// The created reader assumes ownership of objects (such as file descriptors) within the message.
	/// Because of this, you may only create one ancillary message reader for any ancillary message received from the kernel.
	/// You must also ensure that no other object assumes ownership of the objects within the message.
	pub unsafe fn new(buffer: &'a mut [u8], truncated: bool) -> Self {
		Self { buffer, truncated }
	}

	/// Returns the number of used bytes.
	pub fn len(&self) -> usize {
		self.buffer.len()
	}

	/// Returns `true` if the ancillary data is empty.
	pub fn is_empty(&self) -> bool {
		self.buffer.is_empty()
	}

	/// Is `true` if during a recv operation the ancillary message was truncated.
	///
	/// # Example
	///
	/// ```no_run
	/// use tokio_seqpacket::UnixSeqpacket;
	/// use tokio_seqpacket::ancillary::AncillaryMessageReader;
	/// use std::io::IoSliceMut;
	///
	/// #[tokio::main]
	/// async fn main() -> std::io::Result<()> {
	///     let sock = UnixSeqpacket::connect("/tmp/sock").await?;
	///
	///     let mut ancillary_buffer = [0; 128];
	///
	///     let mut buf = [1; 8];
	///     let mut bufs = &mut [IoSliceMut::new(&mut buf)];
	///     let (_read, ancillary) = sock.recv_vectored_with_ancillary(bufs, &mut ancillary_buffer).await?;
	///
	///     println!("Is truncated: {}", ancillary.is_truncated());
	///     Ok(())
	/// }
	/// ```
	pub fn is_truncated(&self) -> bool {
		self.truncated
	}

	/// Returns the iterator of the control messages.
	pub fn messages(&self) -> AncillaryMessages<'_> {
		AncillaryMessages { buffer: self.buffer, current: None }
	}

	/// Consume the ancillary message to take ownership of the contained objects (such as file descriptors).
	pub fn into_messages(mut self) -> IntoAncillaryMessages<'a> {
		let buffer = std::mem::take(&mut self.buffer);
		IntoAncillaryMessages { buffer, current: None }
	}
}

impl Drop for AncillaryMessageReader<'_> {
	fn drop(&mut self) {
		if !self.is_empty() {
			drop(IntoAncillaryMessages { buffer: self.buffer, current: None })
		}
	}
}

impl<'a> Iterator for AncillaryMessages<'a> {
	type Item = AncillaryMessage<'a>;

	fn next(&mut self) -> Option<Self::Item> {
		if self.buffer.is_empty() {
			return None;
		}
		unsafe {
			let mut msg: libc::msghdr = std::mem::zeroed();
			msg.msg_control = self.buffer.as_ptr() as *mut _;
			msg.msg_controllen = self.buffer.len() as _;

			let cmsg = if let Some(current) = self.current {
				libc::CMSG_NXTHDR(&msg, current)
			} else {
				libc::CMSG_FIRSTHDR(&msg)
			};

			let cmsg = cmsg.as_ref()?;

			// Most operating systems, but not Linux or emscripten, return the previous pointer
			// when its length is zero. Therefore, check if the previous pointer is the same as
			// the current one.
			if let Some(current) = self.current {
				if std::ptr::eq(current, cmsg) {
					return None;
				}
			}

			self.current = Some(cmsg);
			let ancillary_result = AncillaryMessage::try_from_cmsghdr(cmsg);
			Some(ancillary_result)
		}
	}
}

impl<'a> AncillaryMessage<'a> {
	#[allow(clippy::unnecessary_cast)]
	fn try_from_cmsghdr(cmsg: &'a libc::cmsghdr) -> Self {
		unsafe {
			let cmsg_len_zero = libc::CMSG_LEN(0) as usize;
			let data_len = cmsg.cmsg_len as usize - cmsg_len_zero;
			let data = libc::CMSG_DATA(cmsg).cast();
			let data = std::slice::from_raw_parts(data, data_len);

			match (cmsg.cmsg_level, cmsg.cmsg_type) {
				(libc::SOL_SOCKET, libc::SCM_RIGHTS) => Self::FileDescriptors(FileDescriptors { data }),
				#[cfg(any(target_os = "android", target_os = "linux", target_os = "netbsd"))]
				(libc::SOL_SOCKET, super::SCM_CREDENTIALS) => Self::Credentials(UnixCredentials { data }),
				(cmsg_level, cmsg_type) => Self::Other(UnknownMessage { cmsg_level, cmsg_type, data }),
			}
		}
	}
}

impl<'a> Iterator for IntoAncillaryMessages<'a> {
	type Item = OwnedAncillaryMessage<'a>;

	fn next(&mut self) -> Option<Self::Item> {
		if self.buffer.is_empty() {
			return None;
		}
		unsafe {
			let mut msg: libc::msghdr = std::mem::zeroed();
			msg.msg_control = self.buffer.as_ptr() as *mut _;
			msg.msg_controllen = self.buffer.len() as _;

			let cmsg = if let Some(current) = self.current {
				libc::CMSG_NXTHDR(&msg, current)
			} else {
				libc::CMSG_FIRSTHDR(&msg)
			};

			let cmsg = cmsg.as_ref()?;

			// Most operating systems, but not Linux or emscripten, return the previous pointer
			// when its length is zero. Therefore, check if the previous pointer is the same as
			// the current one.
			if let Some(current) = self.current {
				if std::ptr::eq(current, cmsg) {
					return None;
				}
			}

			self.current = Some(cmsg);
			let ancillary_result = OwnedAncillaryMessage::try_from_cmsghdr(cmsg);
			Some(ancillary_result)
		}
	}
}

impl Drop for IntoAncillaryMessages<'_> {
	fn drop(&mut self) {
		for message in self {
			drop(message)
		}
	}
}

impl<'a> OwnedAncillaryMessage<'a> {
	#[allow(clippy::unnecessary_cast)]
	fn try_from_cmsghdr(cmsg: &'a libc::cmsghdr) -> Self {
		unsafe {
			let cmsg_len_zero = libc::CMSG_LEN(0) as usize;
			let data_len = cmsg.cmsg_len as usize - cmsg_len_zero;
			let data = libc::CMSG_DATA(cmsg).cast();
			let data = std::slice::from_raw_parts_mut(data, data_len);

			match (cmsg.cmsg_level, cmsg.cmsg_type) {
				(libc::SOL_SOCKET, libc::SCM_RIGHTS) => Self::FileDescriptors(OwnedFileDescriptors { data }),
				#[cfg(any(target_os = "android", target_os = "linux", target_os = "netbsd"))]
				(libc::SOL_SOCKET, super::SCM_CREDENTIALS) => Self::Credentials(UnixCredentials { data }),
				(cmsg_level, cmsg_type) => Self::Other(UnknownMessage { cmsg_level, cmsg_type, data }),
			}
		}
	}
}

impl<'a> FileDescriptors<'a> {
	/// Get the number of file descriptors in the message.
	pub fn len(&self) -> usize {
		self.data.len() / FD_SIZE
	}

	/// Check if the message is empty (contains no file descriptors).
	pub fn is_empty(&self) -> bool {
		self.len() == 0
	}

	/// Get a borrowed file descriptor from the message.
	///
	/// Returns `None` if the index is out of bounds.
	pub fn get(&self, index: usize) -> Option<BorrowedFd<'a>> {
		if index >= self.len() {
			None
		} else {
			// SAFETY: The memory is valid, and the kernel guaranteed it is a file descriptor.
			// Additionally, the returned lifetime is linked to the `AncillaryMessageReader` which owns the file descriptor.
			unsafe {
				Some(std::ptr::read_unaligned(self.data[index * FD_SIZE..].as_ptr().cast()))
			}
		}
	}
}

impl<'a> Iterator for FileDescriptors<'a> {
	type Item = BorrowedFd<'a>;

	fn next(&mut self) -> Option<Self::Item> {
		let fd = self.get(0)?;
		self.data = &self.data[FD_SIZE..];
		Some(fd)
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		(self.len(), Some(self.len()))
	}
}

impl<'a> std::iter::ExactSizeIterator for FileDescriptors<'a> {
	fn len(&self) -> usize {
		self.len()
	}
}

impl<'a> OwnedFileDescriptors<'a> {
	/// Get the number of file descriptors in the message.
	pub fn len(&self) -> usize {
		self.data.len() / FD_SIZE
	}

	/// Check if the message is empty (contains no file descriptors).
	pub fn is_empty(&self) -> bool {
		self.len() == 0
	}

	/// Advance the iterator.
	fn advance(&mut self) {
		let data = std::mem::take(&mut self.data);
		self.data = &mut data[FD_SIZE..];
	}
}

impl<'a> Iterator for OwnedFileDescriptors<'a> {
	type Item = OwnedFd;

	fn next(&mut self) -> Option<Self::Item> {
		if Self::is_empty(self) {
			None
		} else {
			// SAFETY: The memory is valid, and the kernel guaranteed it is a file descriptor.
			// Additionally, the returned lifetime is linked to the `AncillaryMessageReader` which owns the file descriptor.
			// And we overwrite the original value with -1 before returning the owned fd to ensure we don't try to own it multiple times.
			unsafe {
				use std::os::fd::{FromRawFd, RawFd};
				let raw_fd: RawFd = std::ptr::read_unaligned(self.data.as_mut_ptr().cast());
				self.advance();
				Some(OwnedFd::from_raw_fd(raw_fd))
			}
		}
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		(self.len(), Some(self.len()))
	}
}

impl Drop for OwnedFileDescriptors<'_> {
	fn drop(&mut self) {
		for fd in self {
			drop(fd)
		}
	}
}

impl<'a> std::iter::ExactSizeIterator for OwnedFileDescriptors<'a> {
	fn len(&self) -> usize {
		self.len()
	}
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "netbsd"))]
mod unix_creds_impl {
	use super::UnixCredentials;
	use super::super::RawScmCreds;
	use crate::UCred;

	impl UnixCredentials<'_> {
		/// Get the number of credentials in the message.
		pub fn len(&self) -> usize {
			self.data.len() / std::mem::size_of::<RawScmCreds>()
		}

		/// Check if the message is empty (contains no credentials).
		pub fn is_empty(&self) -> bool {
			self.len() == 0
		}

		/// Get the credentials at a specific index.
		pub fn get(&self, index: usize) -> Option<UCred> {
			if index >= self.len() {
				None
			} else {
				// SAFETY: The memory is valid, and the kernel guaranteed it is a credentials struct.
				// It probably also guarantees alignment, but just in case not, use read_unaligned.
				let raw: RawScmCreds = unsafe {
					std::ptr::read_unaligned(self.data.as_ptr().cast::<RawScmCreds>().add(index))
				};
				Some(UCred::from_scm_creds(raw))
			}
		}
	}

	impl Iterator for UnixCredentials<'_> {
		type Item = UCred;

		fn next(&mut self) -> Option<Self::Item> {
			let fd = self.get(0)?;
			self.data = &self.data[std::mem::size_of::<RawScmCreds>()..];
			Some(fd)
		}

		fn size_hint(&self) -> (usize, Option<usize>) {
			(self.len(), Some(self.len()))
		}
	}

	impl<'a> std::iter::ExactSizeIterator for UnixCredentials<'a> {
		fn len(&self) -> usize {
			self.len()
		}
	}
}

impl<'a> UnknownMessage<'a> {
	/// Get the cmsg_level of the message.
	pub fn cmsg_level(&self) -> i32 {
		self.cmsg_level
	}

	/// Get the cmsg_type of the message.
	pub fn cmsg_type(&self) -> i32 {
		self.cmsg_type
	}

	/// Get the data of the message.
	pub fn data(&self) -> &'a [u8] {
		self.data
	}
}
