#![cfg_attr(sendfd_docs, feature(doc_cfg))]

extern crate libc;
#[cfg(feature = "tokio")]
extern crate tokio;

use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net;
use std::{alloc, io, mem, ptr};
#[cfg(feature = "tokio")]
use tokio::io::Interest;

pub mod changelog;

/// An extension trait that enables sending associated file descriptors along with the data.
pub trait SendWithFd {
    /// Send the bytes and the file descriptors.
    fn send_with_fd(&self, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize>;
}

/// An extension trait that enables receiving associated file descriptors along with the data.
pub trait RecvWithFd {
    /// Receive the bytes and the file descriptors.
    ///
    /// The bytes and the file descriptors are received into the corresponding buffers.
    fn recv_with_fd(&self, bytes: &mut [u8], fds: &mut [RawFd]) -> io::Result<(usize, usize)>;
}

// Replace with `<*const u8>::offset_from` once it is stable.
unsafe fn ptr_offset_from(this: *const u8, origin: *const u8) -> isize {
    isize::wrapping_sub(this as _, origin as _)
}

/// Construct the `libc::msghdr` which is used as an argument to `libc::sendmsg` and
/// `libc::recvmsg`.
///
/// The constructed `msghdr` contains the references to the given `iov` and has sufficient
/// (dynamically allocated) space to store `fd_count` file descriptors delivered as ancillary data.
///
/// # Unsafety
///
/// This function provides a "mostly" safe interface, however it is kept unsafe as its only uses
/// are intended to be in other unsafe code and its implementation itself is also unsafe.
unsafe fn construct_msghdr_for(
    iov: &mut libc::iovec,
    fd_count: usize,
) -> (libc::msghdr, alloc::Layout, usize) {
    let fd_len = mem::size_of::<RawFd>() * fd_count;
    let cmsg_buffer_len = libc::CMSG_SPACE(fd_len as u32) as usize;
    let layout = alloc::Layout::from_size_align(cmsg_buffer_len, mem::align_of::<libc::cmsghdr>());
    let (cmsg_buffer, cmsg_layout) = if let Ok(layout) = layout {
        const NULL_MUT_U8: *mut u8 = ptr::null_mut();
        match alloc::alloc(layout) {
            NULL_MUT_U8 => alloc::handle_alloc_error(layout),
            x => (x as *mut _, layout),
        }
    } else {
        // NB: it is fine to construct such a `Layout` as it is not used for actual allocation,
        // just for the error reporting. Either way this branch is not reachable at all provided a
        // well behaved implementation of `CMSG_SPACE` in the host libc.
        alloc::handle_alloc_error(alloc::Layout::from_size_align_unchecked(
            cmsg_buffer_len,
            mem::align_of::<libc::cmsghdr>(),
        ))
    };

    let mut msghdr = mem::zeroed::<libc::msghdr>();
    msghdr.msg_name = ptr::null_mut();
    msghdr.msg_namelen = 0;
    msghdr.msg_iov = iov as *mut _;
    msghdr.msg_iovlen = 1;
    msghdr.msg_control = cmsg_buffer;
    msghdr.msg_controllen = cmsg_buffer_len as _;

    (msghdr, cmsg_layout, fd_len)
}

/// A common implementation of `sendmsg` that sends provided bytes with ancillary file descriptors
/// over either a datagram or stream unix socket.
fn send_with_fd(socket: RawFd, bs: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    unsafe {
        let mut iov = libc::iovec {
            // NB: this casts *const to *mut, and in doing so we trust the OS to be a good citizen
            // and not mutate our buffer. This is the API we have to live with.
            iov_base: bs.as_ptr() as *const _ as *mut _,
            iov_len: bs.len(),
        };
        let (mut msghdr, cmsg_layout, fd_len) = construct_msghdr_for(&mut iov, fds.len());
        let cmsg_buffer = msghdr.msg_control;

        // Fill cmsg with the file descriptors we are sending.
        let cmsg_header = libc::CMSG_FIRSTHDR(&mut msghdr as *mut _);
        let mut cmsghdr = mem::zeroed::<libc::cmsghdr>();
        cmsghdr.cmsg_level = libc::SOL_SOCKET;
        cmsghdr.cmsg_type = libc::SCM_RIGHTS;
        cmsghdr.cmsg_len = libc::CMSG_LEN(fd_len as u32) as _;

        ptr::write(cmsg_header, cmsghdr);

        let cmsg_data = libc::CMSG_DATA(cmsg_header) as *mut RawFd;
        for (i, fd) in fds.iter().enumerate() {
            ptr::write_unaligned(cmsg_data.add(i), *fd);
        }
        let count = libc::sendmsg(socket, &msghdr as *const _, 0);
        if count < 0 {
            let error = io::Error::last_os_error();
            alloc::dealloc(cmsg_buffer as *mut _, cmsg_layout);
            Err(error)
        } else {
            alloc::dealloc(cmsg_buffer as *mut _, cmsg_layout);
            Ok(count as usize)
        }
    }
}

/// A common implementation of `recvmsg` that receives provided bytes and the ancillary file
/// descriptors over either a datagram or stream unix socket.
fn recv_with_fd(socket: RawFd, bs: &mut [u8], mut fds: &mut [RawFd]) -> io::Result<(usize, usize)> {
    unsafe {
        let mut iov = libc::iovec {
            iov_base: bs.as_mut_ptr() as *mut _,
            iov_len: bs.len(),
        };
        let (mut msghdr, cmsg_layout, _) = construct_msghdr_for(&mut iov, fds.len());
        let cmsg_buffer = msghdr.msg_control;
        let count = libc::recvmsg(socket, &mut msghdr as *mut _, 0);
        if count < 0 {
            let error = io::Error::last_os_error();
            alloc::dealloc(cmsg_buffer as *mut _, cmsg_layout);
            return Err(error);
        }

        // Walk the ancillary data buffer and copy the raw descriptors from it into the output
        // buffer.
        let mut descriptor_count = 0;
        let mut cmsg_header = libc::CMSG_FIRSTHDR(&mut msghdr as *mut _);
        while !cmsg_header.is_null() {
            if (*cmsg_header).cmsg_level == libc::SOL_SOCKET
                && (*cmsg_header).cmsg_type == libc::SCM_RIGHTS
            {
                let data_ptr = libc::CMSG_DATA(cmsg_header);
                let data_offset = ptr_offset_from(data_ptr, cmsg_header as *const _);
                debug_assert!(data_offset >= 0);
                let data_byte_count = (*cmsg_header).cmsg_len as usize - data_offset as usize;
                debug_assert!((*cmsg_header).cmsg_len as isize > data_offset);
                debug_assert!(data_byte_count % mem::size_of::<RawFd>() == 0);
                let rawfd_count = (data_byte_count / mem::size_of::<RawFd>()) as isize;
                let fd_ptr = data_ptr as *const RawFd;
                for i in 0..rawfd_count {
                    if let Some((dst, rest)) = { fds }.split_first_mut() {
                        *dst = ptr::read_unaligned(fd_ptr.offset(i));
                        descriptor_count += 1;
                        fds = rest;
                    } else {
                        // This branch is unreachable. We allocate the ancillary data buffer just
                        // large enough to fit exactly the number of `RawFd`s that are in the `fds`
                        // buffer. It is not possible for the OS to return more of them.
                        //
                        // If this branch ended up being reachable for some reason, it would be
                        // necessary for this branch to close the file descriptors to avoid leaking
                        // resources.
                        //
                        // TODO: consider using unreachable_unchecked
                        unreachable!();
                    }
                }
            }
            cmsg_header = libc::CMSG_NXTHDR(&mut msghdr as *mut _, cmsg_header);
        }

        alloc::dealloc(cmsg_buffer as *mut _, cmsg_layout);
        Ok((count as usize, descriptor_count))
    }
}

impl SendWithFd for net::UnixStream {
    /// Send the bytes and the file descriptors as a stream.
    ///
    /// Neither is guaranteed to be received by the other end in a single chunk and
    /// may arrive entirely independently.
    fn send_with_fd(&self, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
        send_with_fd(self.as_raw_fd(), bytes, fds)
    }
}

#[cfg(feature = "tokio")]
#[cfg_attr(sendfd_docs, doc(cfg(feature = "tokio")))]
impl SendWithFd for tokio::net::UnixStream {
    /// Send the bytes and the file descriptors as a stream.
    ///
    /// Neither is guaranteed to be received by the other end in a single chunk and
    /// may arrive entirely independently.
    fn send_with_fd(&self, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
        self.try_io(Interest::WRITABLE, || send_with_fd(self.as_raw_fd(), bytes, fds))
    }
}

#[cfg(feature = "tokio")]
#[cfg_attr(sendfd_docs, doc(cfg(feature = "tokio")))]
impl SendWithFd for tokio::net::unix::WriteHalf<'_> {
    /// Send the bytes and the file descriptors as a stream.
    ///
    /// Neither is guaranteed to be received by the other end in a single chunk and
    /// may arrive entirely independently.
    fn send_with_fd(&self, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
        let unix_stream: &tokio::net::UnixStream = self.as_ref();
        unix_stream.send_with_fd(bytes, fds)
    }
}

impl SendWithFd for net::UnixDatagram {
    /// Send the bytes and the file descriptors as a single packet.
    ///
    /// It is guaranteed that the bytes and the associated file descriptors will arrive at the same
    /// time, however the receiver end may not receive the full message if its buffers are too
    /// small.
    fn send_with_fd(&self, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
        send_with_fd(self.as_raw_fd(), bytes, fds)
    }
}

#[cfg(feature = "tokio")]
#[cfg_attr(sendfd_docs, doc(cfg(feature = "tokio")))]
impl SendWithFd for tokio::net::UnixDatagram {
    /// Send the bytes and the file descriptors as a single packet.
    ///
    /// It is guaranteed that the bytes and the associated file descriptors will arrive at the same
    /// time, however the receiver end may not receive the full message if its buffers are too
    /// small.
    fn send_with_fd(&self, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
        self.try_io(Interest::WRITABLE, || send_with_fd(self.as_raw_fd(), bytes, fds))
    }
}

impl RecvWithFd for net::UnixStream {
    /// Receive the bytes and the file descriptors from the stream.
    ///
    /// It is not guaranteed that the received information will form a single coherent packet of
    /// data. In other words, it is not required that this receives the bytes and file descriptors
    /// that were sent with a single `send_with_fd` call by somebody else.
    fn recv_with_fd(&self, bytes: &mut [u8], fds: &mut [RawFd]) -> io::Result<(usize, usize)> {
        recv_with_fd(self.as_raw_fd(), bytes, fds)
    }
}

#[cfg(feature = "tokio")]
#[cfg_attr(sendfd_docs, doc(cfg(feature = "tokio")))]
impl RecvWithFd for tokio::net::UnixStream {
    /// Receive the bytes and the file descriptors from the stream.
    ///
    /// It is not guaranteed that the received information will form a single coherent packet of
    /// data. In other words, it is not required that this receives the bytes and file descriptors
    /// that were sent with a single `send_with_fd` call by somebody else.
    fn recv_with_fd(&self, bytes: &mut [u8], fds: &mut [RawFd]) -> io::Result<(usize, usize)> {
        self.try_io(Interest::READABLE, || recv_with_fd(self.as_raw_fd(), bytes, fds))
    }
}

#[cfg(feature = "tokio")]
#[cfg_attr(sendfd_docs, doc(cfg(feature = "tokio")))]
impl RecvWithFd for tokio::net::unix::ReadHalf<'_> {
    /// Receive the bytes and the file descriptors from the stream.
    ///
    /// It is not guaranteed that the received information will form a single coherent packet of
    /// data. In other words, it is not required that this receives the bytes and file descriptors
    /// that were sent with a single `send_with_fd` call by somebody else.
    fn recv_with_fd(&self, bytes: &mut [u8], fds: &mut [RawFd]) -> io::Result<(usize, usize)> {
        let unix_stream: &tokio::net::UnixStream = self.as_ref();
        unix_stream.recv_with_fd(bytes, fds)
    }
}

impl RecvWithFd for net::UnixDatagram {
    /// Receive the bytes and the file descriptors as a single packet.
    ///
    /// It is guaranteed that the received information will form a single coherent packet, and data
    /// received will match a corresponding `send_with_fd` call. Note, however, that in case the
    /// receiving buffer(s) are to small, the message may get silently truncated and the
    /// undelivered data will be discarded.
    ///
    /// For receiving the file descriptors, the internal buffer is sized according to the size of
    /// the `fds` buffer. If the sender sends `fds.len()` descriptors, but prefaces the descriptors
    /// with some other ancilliary data, then some file descriptors may be truncated as well.
    fn recv_with_fd(&self, bytes: &mut [u8], fds: &mut [RawFd]) -> io::Result<(usize, usize)> {
        recv_with_fd(self.as_raw_fd(), bytes, fds)
    }
}

#[cfg(feature = "tokio")]
#[cfg_attr(sendfd_docs, doc(cfg(feature = "tokio")))]
impl RecvWithFd for tokio::net::UnixDatagram {
    /// Receive the bytes and the file descriptors as a single packet.
    ///
    /// It is guaranteed that the received information will form a single coherent packet, and data
    /// received will match a corresponding `send_with_fd` call. Note, however, that in case the
    /// receiving buffer(s) are to small, the message may get silently truncated and the
    /// undelivered data will be discarded.
    ///
    /// For receiving the file descriptors, the internal buffer is sized according to the size of
    /// the `fds` buffer. If the sender sends `fds.len()` descriptors, but prefaces the descriptors
    /// with some other ancilliary data, then some file descriptors may be truncated as well.
    fn recv_with_fd(&self, bytes: &mut [u8], fds: &mut [RawFd]) -> io::Result<(usize, usize)> {
        self.try_io(Interest::READABLE, || recv_with_fd(self.as_raw_fd(), bytes, fds))
    }
}

#[cfg(test)]
mod tests {
    use super::{RecvWithFd, SendWithFd};
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use std::os::unix::net;

    #[test]
    fn stream_works() {
        let (l, r) = net::UnixStream::pair().expect("create UnixStream pair");
        let sent_bytes = b"hello world!";
        let sent_fds = [l.as_raw_fd(), r.as_raw_fd()];
        assert_eq!(
            l.send_with_fd(&sent_bytes[..], &sent_fds[..])
                .expect("send should be successful"),
            sent_bytes.len()
        );
        let mut recv_bytes = [0; 128];
        let mut recv_fds = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            r.recv_with_fd(&mut recv_bytes, &mut recv_fds)
                .expect("recv should be successful"),
            (sent_bytes.len(), sent_fds.len())
        );
        assert_eq!(recv_bytes[..sent_bytes.len()], sent_bytes[..]);
        for (&sent, &recvd) in sent_fds.iter().zip(&recv_fds[..]) {
            // Modify the sent resource and check if the received resource has been modified the
            // same way.
            let expected_value = Some(std::time::Duration::from_secs(42));
            unsafe {
                let s = net::UnixStream::from_raw_fd(sent);
                s.set_read_timeout(expected_value)
                    .expect("set read timeout");
                std::mem::forget(s);
                assert_eq!(
                    net::UnixStream::from_raw_fd(recvd)
                        .read_timeout()
                        .expect("get read timeout"),
                    expected_value
                );
            }
        }
    }

    #[test]
    fn datagram_works() {
        let (l, r) = net::UnixDatagram::pair().expect("create UnixDatagram pair");
        let sent_bytes = b"hello world!";
        let sent_fds = [l.as_raw_fd(), r.as_raw_fd()];
        assert_eq!(
            l.send_with_fd(&sent_bytes[..], &sent_fds[..])
                .expect("send should be successful"),
            sent_bytes.len()
        );
        let mut recv_bytes = [0; 128];
        let mut recv_fds = [0, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            r.recv_with_fd(&mut recv_bytes, &mut recv_fds)
                .expect("recv should be successful"),
            (sent_bytes.len(), sent_fds.len())
        );
        assert_eq!(recv_bytes[..sent_bytes.len()], sent_bytes[..]);
        for (&sent, &recvd) in sent_fds.iter().zip(&recv_fds[..]) {
            // Modify the sent resource and check if the received resource has been modified the
            // same way.
            let expected_value = Some(std::time::Duration::from_secs(42));
            unsafe {
                let s = net::UnixDatagram::from_raw_fd(sent);
                s.set_read_timeout(expected_value)
                    .expect("set read timeout");
                std::mem::forget(s);
                assert_eq!(
                    net::UnixDatagram::from_raw_fd(recvd)
                        .read_timeout()
                        .expect("get read timeout"),
                    expected_value
                );
            }
        }
    }

    #[test]
    fn datagram_works_across_processes() {
        let (l, r) = net::UnixDatagram::pair().expect("create UnixDatagram pair");
        let sent_bytes = b"hello world!";
        let sent_fds = [l.as_raw_fd(), r.as_raw_fd()];

        unsafe {
            match libc::fork() {
                -1 => panic!("fork failed!"),
                0 => {
                    // This is the child in which we attempt to send a file descriptor back to
                    // parent, emulating the cross-process FD sharing.
                    l.send_with_fd(&sent_bytes[..], &sent_fds[..])
                        .expect("send should be successful");
                    ::std::process::exit(0);
                }
                _ => {
                    // Parent process, receives the file descriptors sent by forked child.
                }
            }
            let mut recv_bytes = [0; 128];
            let mut recv_fds = [0, 0, 0, 0, 0, 0, 0];
            assert_eq!(
                r.recv_with_fd(&mut recv_bytes, &mut recv_fds)
                    .expect("recv should be successful"),
                (sent_bytes.len(), sent_fds.len())
            );
            assert_eq!(recv_bytes[..sent_bytes.len()], sent_bytes[..]);
            for (&sent, &recvd) in sent_fds.iter().zip(&recv_fds[..]) {
                // Modify the sent resource and check if the received resource has been
                // modified the same way.
                let expected_value = Some(std::time::Duration::from_secs(42));
                let s = net::UnixDatagram::from_raw_fd(sent);
                s.set_read_timeout(expected_value)
                    .expect("set read timeout");
                std::mem::forget(s);
                assert_eq!(
                    net::UnixDatagram::from_raw_fd(recvd)
                        .read_timeout()
                        .expect("get read timeout"),
                    expected_value
                );
            }
        }
    }

    #[test]
    fn sending_junk_fails() {
        let (l, _) = net::UnixDatagram::pair().expect("create UnixDatagram pair");
        let sent_bytes = b"hello world!";
        if let Ok(_) = l.send_with_fd(&sent_bytes[..], &[i32::max_value()][..]) {
            panic!("expected an error when sending a junk file descriptor");
        }
        if let Ok(_) = l.send_with_fd(&sent_bytes[..], &[0xffi32][..]) {
            panic!("expected an error when sending a junk file descriptor");
        }
    }
}
