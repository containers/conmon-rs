use filedesc::FileDesc;
use std::convert::TryInto;
use std::io::{IoSlice, IoSliceMut};
use std::os::raw::{c_int, c_void};
use std::path::{Path, PathBuf};

use crate::ancillary::{AncillaryMessageReader, AncillaryMessageWriter};

const SOCKET_FLAGS: c_int = libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
const SOCKET_TYPE: c_int = libc::SOCK_SEQPACKET | SOCKET_FLAGS;
const SEND_MSG_DEFAULT_FLAGS: c_int = libc::MSG_NOSIGNAL;

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
const RECV_MSG_DEFAULT_FLAGS: c_int = libc::MSG_NOSIGNAL;
#[cfg(not(any(target_os = "illumos", target_os = "solaris")))]
const RECV_MSG_DEFAULT_FLAGS: c_int = libc::MSG_NOSIGNAL | libc::MSG_CMSG_CLOEXEC;

pub fn local_seqpacket_socket() -> std::io::Result<FileDesc> {
	unsafe {
		let fd = check(libc::socket(libc::AF_UNIX, SOCKET_TYPE, 0))?;
		Ok(FileDesc::from_raw_fd(fd))
	}
}

pub fn local_seqpacket_pair() -> std::io::Result<(FileDesc, FileDesc)> {
	unsafe {
		let mut fds: [c_int; 2] = [0, 0];
		check(libc::socketpair(libc::AF_UNIX, SOCKET_TYPE, 0, fds.as_mut_ptr()))?;
		Ok((FileDesc::from_raw_fd(fds[0]), FileDesc::from_raw_fd(fds[1])))
	}
}

pub fn connect<P: AsRef<Path>>(socket: &FileDesc, address: P) -> std::io::Result<()> {
	let (address, addr_len) = path_to_sockaddr(address.as_ref())?;
	unsafe {
		check(libc::connect(
			socket.as_raw_fd(),
			&address as *const _ as *const libc::sockaddr,
			addr_len as _,
		))?;
		Ok(())
	}
}

pub fn bind<P: AsRef<Path>>(socket: &FileDesc, address: P) -> std::io::Result<()> {
	let (address, addr_len) = path_to_sockaddr(address.as_ref())?;
	unsafe {
		check(libc::bind(
			socket.as_raw_fd(),
			&address as *const _ as *const _,
			addr_len as _,
		))?;
		Ok(())
	}
}

pub fn listen(socket: &FileDesc, backlog: c_int) -> std::io::Result<()> {
	unsafe {
		check(libc::listen(socket.as_raw_fd(), backlog))?;
		Ok(())
	}
}

pub fn accept(socket: &FileDesc) -> std::io::Result<FileDesc> {
	unsafe {
		let mut addr: libc::sockaddr_un = core::mem::zeroed();
		let mut addr_len: libc::socklen_t = 0;
		let fd = check(libc::accept4(
			socket.as_raw_fd(),
			&mut addr as *mut _ as *mut _,
			&mut addr_len,
			SOCKET_FLAGS,
		))?;
		Ok(FileDesc::from_raw_fd(fd))
	}
}

pub fn shutdown(socket: &FileDesc, how: std::net::Shutdown) -> std::io::Result<()> {
	let how = match how {
		std::net::Shutdown::Read => libc::SHUT_RD,
		std::net::Shutdown::Write => libc::SHUT_WR,
		std::net::Shutdown::Both => libc::SHUT_RDWR,
	};
	unsafe {
		check(libc::shutdown(socket.as_raw_fd(), how))?;
		Ok(())
	}
}

pub fn take_socket_error(socket: &FileDesc) -> std::io::Result<Option<std::io::Error>> {
	unsafe {
		let mut error: c_int = 0;
		let mut len = core::mem::size_of::<c_int>() as libc::socklen_t;
		check(libc::getsockopt(
			socket.as_raw_fd(),
			libc::SOL_SOCKET,
			libc::SO_ERROR,
			&mut error as *mut c_int as *mut c_void,
			&mut len,
		))?;
		if error == 0 {
			Ok(None)
		} else {
			Ok(Some(std::io::Error::from_raw_os_error(error)))
		}
	}
}

pub fn get_local_address(socket: &FileDesc) -> std::io::Result<PathBuf> {
	unsafe {
		let mut addr: libc::sockaddr_un = core::mem::zeroed();
		let mut len = core::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
		check(libc::getsockname(
			socket.as_raw_fd(),
			&mut addr as *mut _ as *mut _,
			&mut len,
		))?;
		Ok(sockaddr_to_path(&addr, len)?.to_path_buf())
	}
}

pub fn send(socket: &FileDesc, buffer: &[u8]) -> std::io::Result<usize> {
	unsafe {
		check_size(libc::send(
			socket.as_raw_fd(),
			buffer.as_ptr() as *const c_void,
			buffer.len(),
			SEND_MSG_DEFAULT_FLAGS,
		))
	}
}

pub fn send_msg(socket: &FileDesc, buffer: &[IoSlice], ancillary: &mut AncillaryMessageWriter) -> std::io::Result<usize> {
	let control_data = match ancillary.len() {
		0 => std::ptr::null_mut(),
		_ => ancillary.buffer.as_mut_ptr() as *mut std::os::raw::c_void,
	};

	let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
	header.msg_name = std::ptr::null_mut();
	header.msg_namelen = 0;
	header.msg_iov = buffer.as_ptr() as *mut libc::iovec;
	// This is not a no-op on all platforms.
	#[allow(clippy::useless_conversion)]
	{
		header.msg_iovlen = buffer.len().try_into().map_err(|_| std::io::ErrorKind::InvalidInput)?;
	}
	header.msg_flags = 0;
	header.msg_control = control_data;
	// This is not a no-op on all platforms.
	#[allow(clippy::useless_conversion)]
	{
		header.msg_controllen = ancillary
			.len()
			.try_into()
			.map_err(|_| std::io::ErrorKind::InvalidInput)?;
	}

	unsafe {
		check_size(libc::sendmsg(
			socket.as_raw_fd(),
			&header as *const _,
			SEND_MSG_DEFAULT_FLAGS,
		))
	}
}

pub fn recv(socket: &FileDesc, buffer: &mut [u8]) -> std::io::Result<usize> {
	unsafe {
		let read = check_size(libc::recv(
			socket.as_raw_fd(),
			buffer.as_mut_ptr() as *mut c_void,
			buffer.len(),
			RECV_MSG_DEFAULT_FLAGS,
		))?;
		Ok(read)
	}
}

pub fn recv_msg<'a>(
	socket: &FileDesc,
	buffer: &mut [IoSliceMut],
	ancillary_buffer: &'a mut [u8],
) -> std::io::Result<(usize, AncillaryMessageReader<'a>)> {
	let control_data = match ancillary_buffer.len() {
		0 => std::ptr::null_mut(),
		_ => ancillary_buffer.as_mut_ptr() as *mut std::os::raw::c_void,
	};

	let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
	header.msg_name = std::ptr::null_mut();
	header.msg_namelen = 0;
	header.msg_iov = buffer.as_ptr() as *mut libc::iovec;
	// This is not a no-op on all platforms.
	#[allow(clippy::useless_conversion)]
	{
		header.msg_iovlen = buffer.len().try_into().map_err(|_| std::io::ErrorKind::InvalidInput)?;
	}
	header.msg_flags = 0;
	header.msg_control = control_data;
	// This is not a no-op on all platforms.
	#[allow(clippy::useless_conversion)]
	{
		header.msg_controllen = ancillary_buffer.len()
			.try_into()
			.map_err(|_| std::io::ErrorKind::InvalidInput)?;
	}

	let size = unsafe {
		check_size(libc::recvmsg(
			socket.as_raw_fd(),
			&mut header as *mut _,
			RECV_MSG_DEFAULT_FLAGS,
		))?
	};
	let truncated = header.msg_flags & libc::MSG_CTRUNC != 0;
	// This is not a no-op on all platforms.
	#[allow(clippy::unnecessary_cast)]
	let length = header.msg_controllen as usize;

	let ancillary_reader = unsafe { AncillaryMessageReader::new(&mut ancillary_buffer[..length], truncated) };

	#[cfg(any(target_os = "illumos", target_os = "solaris"))]
	post_process_fds(&ancillary_reader);
	Ok((size, ancillary_reader))
}

// Illumos and solaris do not support MSG_CMSG_CLOEXEC,
// so we fix-up all received file descriptors manually.
#[cfg(any(target_os = "illumos", target_os = "solaris"))]
fn post_process_fds(ancillary: &AncillaryMessageReader) {
	use std::os::fd::AsRawFd;

	for cmsg in ancillary.messages() {
		if let crate::ancillary::AncillaryMessage::FileDescriptors(fds) = cmsg {
			for fd in fds {
				// Safety: the file descriptor is guaranteed to be valid by `BorrowedFd`.
				// And because the `FileDesc` is wrapped in a `ManuallyDrop`, we never close it here.
				let fd = core::mem::ManuallyDrop::new(unsafe { FileDesc::from_raw_fd(fd.as_raw_fd()) });
				fd.set_close_on_exec(true).ok();
			}
		}
	}
}

fn path_to_sockaddr(path: &Path) -> std::io::Result<(libc::sockaddr_un, usize)> {
	use std::os::unix::ffi::OsStrExt;
	let path = path.as_os_str().as_bytes();
	unsafe {
		let mut sockaddr: libc::sockaddr_un = core::mem::zeroed();
		let max_len = core::mem::size_of_val(&sockaddr.sun_path) - 1;

		if path.len() > max_len {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"path length exceeds maximum sockaddr length",
			));
		}

		sockaddr.sun_family = libc::AF_UNIX as _;
		core::ptr::copy_nonoverlapping(path.as_ptr(), sockaddr.sun_path.as_mut_ptr() as *mut u8, path.len());
		sockaddr.sun_path[path.len()] = 0;
		let path_offset = sockaddr.sun_path.as_ptr() as usize - (&sockaddr as *const _ as usize);
		Ok((sockaddr, path_offset + path.len() + 1))
	}
}

/// Get the Unix path of a socket address.
///
/// An error is returned if the address is not a Unix address, or if it is an unnamed or abstract.
fn sockaddr_to_path(address: &libc::sockaddr_un, len: libc::socklen_t) -> std::io::Result<&std::path::Path> {
	use std::ffi::OsStr;
	use std::os::unix::ffi::OsStrExt;

	if address.sun_family != libc::AF_LOCAL as _ {
		Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			format!("address family is not AF_LOCAL/UNIX: {}", address.sun_family),
		))
	} else {
		unsafe {
			let address: &libc::sockaddr_un = std::mem::transmute(address);
			let sun_path: *const u8 = address.sun_path.as_ptr().cast();
			let offset = sun_path.offset_from(address as *const _ as *const u8);
			let path = core::slice::from_raw_parts(sun_path, len as usize - offset as usize);

			// Some platforms include a trailing null byte in the path length.
			let path = if path.last() == Some(&0) {
				&path[..path.len() - 1]
			} else {
				path
			};
			Ok(Path::new(OsStr::from_bytes(path)))
		}
	}
}

/// Check the return value of a syscall that returns a size.
fn check_size(ret: isize) -> std::io::Result<usize> {
	if ret < 0 {
		Err(std::io::Error::last_os_error())
	} else {
		Ok(ret as usize)
	}
}

/// Check the return value of a syscall.
fn check(value: std::os::raw::c_int) -> std::io::Result<std::os::raw::c_int> {
	if value == -1 {
		Err(std::io::Error::last_os_error())
	} else {
		Ok(value)
	}
}
