//! Support for reading / writing ancillary data.

use std::os::fd::BorrowedFd;

mod reader;
pub use reader::*;

mod writer;
pub use writer::{AncillaryMessageWriter, AddControlMessageError};

const FD_SIZE: usize = std::mem::size_of::<BorrowedFd>();

#[cfg(any(target_os = "android", target_os = "linux"))]
pub(crate) type RawScmCreds = libc::ucred;

#[cfg(target_os = "netbsd")]
#[repr(C)]
// Custom type for NetBSD socketcred.
// Source:
// * https://man.netbsd.org/NetBSD-8.0/unix.4
// * https://man.netbsd.org/NetBSD-9.0/unix.4
// * https://man.netbsd.org/NetBSD-10.0-STABLE/unix.4
//
// But set size of sc_groups array to 0, because we never fill it in.
pub(crate) struct RawScmCreds {
	pub sc_pid: libc::pid_t,
	pub sc_uid: libc::uid_t,
	pub sc_euid: libc::uid_t,
	pub sc_gid: libc::gid_t,
	pub sc_egid: libc::gid_t,
	pub sc_ngroups: libc::c_int,
	pub sc_groups: [libc::gid_t; 0],
}

#[cfg(any(target_os = "android", target_os = "linux"))]
const SCM_CREDENTIALS: libc::c_int = libc::SCM_CREDENTIALS;

#[cfg(target_os = "netbsd")]
const SCM_CREDENTIALS: libc::c_int = libc::SCM_CREDS;
