use crate::borrow_fd::BorrowFd;

use super::FD_SIZE;

/// Writer to help you construct ancillary messages for Unix sockets.
///
/// The writer uses a pre-allocated buffer and will never (re)-allocate.
///
/// # Example
/// ```no_run
/// use tokio_seqpacket::UnixSeqpacket;
/// use tokio_seqpacket::ancillary::AncillaryMessageWriter;
/// use std::io::IoSlice;
/// use std::os::fd::AsRawFd;
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let sock = UnixSeqpacket::connect("/tmp/sock").await?;
///     let file = std::fs::File::create("/tmp/my-file")?;
///
///     let mut fds = [0; 8];
///     let mut ancillary_buffer = [0; 128];
///     let mut ancillary = AncillaryMessageWriter::new(&mut ancillary_buffer);
///     ancillary.add_fds(&[&file])?;
///
///     let mut buf = [1; 8];
///     let mut bufs = [IoSlice::new(&mut buf)];
///     sock.send_vectored_with_ancillary(&mut bufs, &mut ancillary).await?;
///
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct AncillaryMessageWriter<'a> {
	pub(crate) buffer: &'a mut [u8],
	pub(crate) length: usize,
}

/// Failed to add a control message to a ancillary message buffer.
pub struct AddControlMessageError(());

impl<'a> AncillaryMessageWriter<'a> {
	/// Alignment requirement for the control messages added to the buffer.
	pub const BUFFER_ALIGN: usize = std::mem::align_of::<libc::cmsghdr>();

	/// Create an ancillary data with the given buffer.
	///
	/// Some bytes at the start of the buffer may be left unused to enforce alignment to [`Self::BUFFER_ALIGN`].
	/// You can use [`Self::capacity()`] to check how much of the buffer can be used for control messages.
	///
	/// # Example
	///
	/// ```no_run
	/// # #![allow(unused_mut)]
	/// use tokio_seqpacket::ancillary::AncillaryMessageWriter;
	/// let mut ancillary_buffer = [0; 128];
	/// let mut ancillary = AncillaryMessageWriter::new(&mut ancillary_buffer);
	/// ```
	pub fn new(buffer: &'a mut [u8]) -> Self {
		let buffer = align_buffer_mut(buffer, Self::BUFFER_ALIGN);
		Self { buffer, length: 0 }
	}

	/// Returns the capacity of the buffer.
	pub fn capacity(&self) -> usize {
		self.buffer.len()
	}

	/// Returns `true` if the ancillary data is empty.
	pub fn is_empty(&self) -> bool {
		self.length == 0
	}

	/// Returns the number of used bytes.
	pub fn len(&self) -> usize {
		self.length
	}

	/// Add file descriptors to the ancillary data.
	///
	/// The function returns `Ok(())` if there was enough space in the buffer.
	/// If there was not enough space then no file descriptors was appended.
	///
	/// This adds a single control message with level `SOL_SOCKET` and type `SCM_RIGHTS`.
	///
	/// # Example
	///
	/// ```no_run
	/// use tokio_seqpacket::UnixSeqpacket;
	/// use tokio_seqpacket::ancillary::AncillaryMessageWriter;
	/// use std::os::unix::io::AsFd;
	/// use std::io::IoSlice;
	///
	/// #[tokio::main]
	/// async fn main() -> std::io::Result<()> {
	///     let sock = UnixSeqpacket::connect("/tmp/sock").await?;
	///     let file = std::fs::File::open("/my/file")?;
	///
	///     let mut ancillary_buffer = [0; 128];
	///     let mut ancillary = AncillaryMessageWriter::new(&mut ancillary_buffer);
	///     ancillary.add_fds(&[file.as_fd()]);
	///
	///     let buf = [1; 8];
	///     let mut bufs = &mut [IoSlice::new(&buf)];
	///     sock.send_vectored_with_ancillary(bufs, &mut ancillary).await?;
	///     Ok(())
	/// }
	/// ```
	pub fn add_fds<T>(&mut self, fds: &[T]) -> Result<(), AddControlMessageError>
		where
			T: BorrowFd<'a>,
	{
		use std::os::fd::AsRawFd;

		let byte_len = fds.len() * FD_SIZE;
		let buffer = reserve_ancillary_data(self.buffer, &mut self.length, byte_len, libc::SOL_SOCKET, libc::SCM_RIGHTS)?;

		for (i, fd) in fds.iter().enumerate() {
			let bytes = fd.borrow_fd().as_raw_fd().to_ne_bytes();
			buffer[i * FD_SIZE..][..FD_SIZE].copy_from_slice(&bytes)
		}
		Ok(())
	}

	/// Add Unix credentials to the ancillary data.
	///
	/// The function returns `Ok(())` if there is enough space in the buffer.
	/// If there is not enough space, then no credentials are appended.
	///
	/// This function adds a single control message with level `SOL_SOCKET` and type `SCM_CREDENTIALS` on most platforms.
	/// On NetBSD the message has type `SCM_CREDS`.
	#[cfg(any(target_os = "android", target_os = "linux", target_os = "netbsd"))]
	pub fn add_ucreds(&mut self, credentials: &[crate::UCred]) -> Result<(), AddControlMessageError> {
		use super::RawScmCreds;

		const ELEM_SIZE: usize = std::mem::size_of::<RawScmCreds>();

		let byte_len = credentials.len() * ELEM_SIZE;
		let buffer = reserve_ancillary_data(self.buffer, &mut self.length, byte_len, libc::SOL_SOCKET, super::SCM_CREDENTIALS)?;

		for (i, cred) in credentials.iter().enumerate() {
			let raw = &cred.to_scm_creds();
			// SAFETY: The pointers are guaranteed valid and non-overlapping,
			// since they come from distinct &mut self and &[SocketCred] references.
			// The buffer is guaranteed to be large enough by `reserve_ancillary_data`.
			unsafe {
				std::ptr::copy_nonoverlapping(raw as *const _ as *const u8, buffer[i * ELEM_SIZE..].as_mut_ptr(), ELEM_SIZE);
			}
		}
		Ok(())
	}

	/// Add Unix credentials to the ancillary data.
	///
	/// The function returns `Ok(())` if there is enough space in the buffer.
	/// If there is not enough space, then no credentials are appended.
	///
	/// This function adds a single control message with level `SOL_SOCKET` and type `SCM_CREDENTIALS` on most platforms.
	/// On NetBSD the message has type `SCM_CREDS`.
	#[cfg(all(doc, not(any(target_os = "android", target_os = "linux", target_os = "netbsd"))))]
	pub fn add_ucreds(&mut self, credentials: &[crate::UCred]) -> Result<(), AddControlMessageError> {
		panic!("fake function for doc generation")
	}
}

impl std::error::Error for AddControlMessageError {}

impl std::fmt::Debug for AddControlMessageError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("AddDataError").finish()
	}
}

impl std::fmt::Display for AddControlMessageError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("Not enough space in ancillary buffer")
	}
}

impl From<AddControlMessageError> for std::io::Error {
	fn from(_value: AddControlMessageError) -> Self {
		std::io::Error::from_raw_os_error(libc::ENOSPC)
	}
}

fn reserve_ancillary_data<'a>(
	buffer: &'a mut [u8],
	length: &mut usize,
	byte_len: usize,
	cmsg_level: libc::c_int,
	cmsg_type: libc::c_int,
) -> Result<&'a mut [u8], AddControlMessageError> {
	let byte_len = u32::try_from(byte_len)
		.map_err(|_| AddControlMessageError(()))?;

	unsafe {
		let additional_space = libc::CMSG_SPACE(byte_len) as usize;
		let new_length = length.checked_add(additional_space)
			.ok_or(AddControlMessageError(()))?;
		if new_length > buffer.len() {
			return Err(AddControlMessageError(()));
		}

		buffer[*length..new_length].fill(0);

		let mut msg: libc::msghdr = std::mem::zeroed();
		msg.msg_control = buffer.as_mut_ptr().cast();
		msg.msg_controllen = new_length as _;

		let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
		let mut previous_cmsg = cmsg;
		while !cmsg.is_null() {
			previous_cmsg = cmsg;
			cmsg = libc::CMSG_NXTHDR(&msg, cmsg);

			// Most operating systems, but not Linux or emscripten, return the previous pointer
			// when its length is zero. Therefore, check if the previous pointer is the same as
			// the current one.
			if std::ptr::eq(cmsg, previous_cmsg) {
				break;
			}
		}

		if previous_cmsg.is_null() {
			return Err(AddControlMessageError(()));
		}

		*length = new_length;
		(*previous_cmsg).cmsg_level = cmsg_level;
		(*previous_cmsg).cmsg_type = cmsg_type;
		(*previous_cmsg).cmsg_len = libc::CMSG_LEN(byte_len) as _;

		let data = libc::CMSG_DATA(previous_cmsg).cast();
		Ok(std::slice::from_raw_parts_mut(data, additional_space))
	}
}

/// Align a buffer to the given alignment.
fn align_buffer_mut(buffer: &mut [u8], align: usize) -> &mut [u8] {
	let offset = buffer.as_ptr().align_offset(align);
	if offset > buffer.len() {
		&mut []
	} else {
		&mut buffer[offset..]
	}
}
