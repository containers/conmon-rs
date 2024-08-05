use assert2::{assert, let_assert};
use std::io::Read;
use tokio_seqpacket::ancillary::{OwnedAncillaryMessage, AncillaryMessage};

mod ancillary_fd_helper;
use ancillary_fd_helper::receive_file_descriptor;

#[tokio::test]
async fn pass_fd() {
	// Receive a file descriptor
	let mut ancillary_buffer = [0; 64];
	let ancillary = receive_file_descriptor(&mut ancillary_buffer).await;

	// Check that we got exactly one control message containing file descriptors.
	let mut messages = ancillary.messages();
	let_assert!(Some(AncillaryMessage::FileDescriptors(mut fds)) = messages.next());
	assert!(let None = messages.next());

	// Check that we got exactly one file descriptor in the first control message.
	let_assert!(Some(fd) = fds.next());
	assert!(let None = fds.next());

	// Check that we can retrieve the message from the attached file.
	let_assert!(Ok(fd) = fd.try_clone_to_owned());
	let mut file = std::fs::File::from(fd);
	let mut contents = Vec::new();
	assert!(let Ok(_) = file.read_to_end(&mut contents));
	assert!(contents == b"Wie dit leest is gek.");
}

#[tokio::test]
async fn can_take_ownership_of_received_fds() {
	// Receive a file descriptor
	let mut ancillary_buffer = [0; 64];
	let ancillary = receive_file_descriptor(&mut ancillary_buffer).await;

	// Take ownership of the file descriptors.
	let mut messages = ancillary.into_messages();
	let_assert!(Some(OwnedAncillaryMessage::FileDescriptors(mut fds)) = messages.next());
	let_assert!(None = messages.next());
	assert!(fds.len() == 1);
	let_assert!(Some(fd) = fds.next());
	let_assert!(None = fds.next());

	// Check that we can retrieve the message from the attached file.
	let mut file = std::fs::File::from(fd);
	let mut contents = Vec::new();
	assert!(let Ok(_) = file.read_to_end(&mut contents));
	assert!(contents == b"Wie dit leest is gek.");
}

#[tokio::test]
async fn pass_fd_unaligned_buffer() {
	use tokio_seqpacket::ancillary::AncillaryMessageWriter;

	// Receive a file descriptor
	let mut ancillary_buffer = [0; 64];
	// But use a purposefully misaligned ancillary buffer.
	let align = ancillary_buffer.as_ptr().align_offset(AncillaryMessageWriter::BUFFER_ALIGN);
	let ancillary = receive_file_descriptor(&mut ancillary_buffer[align + 1..]).await;

	// Check that we got exactly one control message containing file descriptors.
	let mut messages = ancillary.messages();
	let_assert!(Some(AncillaryMessage::FileDescriptors(mut fds)) = messages.next());
	assert!(let None = messages.next());

	// Check that we got exactly one file descriptor in the first control message.
	let_assert!(Some(fd) = fds.next());
	assert!(let None = fds.next());

	// Check that we can retrieve the message from the attached file.
	let_assert!(Ok(fd) = fd.try_clone_to_owned());
	let mut file = std::fs::File::from(fd);
	let mut contents = Vec::new();
	assert!(let Ok(_) = file.read_to_end(&mut contents));
	assert!(contents == b"Wie dit leest is gek.");
}
