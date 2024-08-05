use assert2::{assert, let_assert};
use std::io::{IoSlice, IoSliceMut, Seek, Write};
use std::os::fd::AsFd;
use tempfile::tempfile;
use tokio_seqpacket::UnixSeqpacket;
use tokio_seqpacket::ancillary::{AncillaryMessageReader, AncillaryMessageWriter};

pub async fn receive_file_descriptor(ancillary_buf: &mut [u8]) -> AncillaryMessageReader<'_> {
	let socket_b = {
		// Make a file to send as attachment.
		let_assert!(Ok(mut file) = tempfile());
		assert!(let Ok(_) = file.write_all(b"Wie dit leest is gek."));
		assert!(let Ok(()) = file.rewind());

		// Make a pair of connected sockets to send it over.
		let_assert!(Ok((socket_a, socket_b)) = UnixSeqpacket::pair());

		// Prepare an ancillary message and add the file descriptor to it.
		let mut cmsg = [0; 64];
		let mut cmsg = AncillaryMessageWriter::new(&mut cmsg);
		assert!(let Ok(()) = cmsg.add_fds(&[file.as_fd()]));

		// Send the message with file descriptor.
		assert!(let Ok(29) = socket_a.send_vectored_with_ancillary(&[IoSlice::new(b"Here, have a file descriptor.")], &mut cmsg).await);

		// Return the receiving socket from the scope.
		socket_b
	};

	let mut read_buf = [0u8; 64];
	let_assert!(Ok((29, cmsg)) = socket_b.recv_vectored_with_ancillary(&mut [IoSliceMut::new(&mut read_buf)], ancillary_buf).await);
	assert!(&read_buf[..29] == b"Here, have a file descriptor.");

	cmsg
}
