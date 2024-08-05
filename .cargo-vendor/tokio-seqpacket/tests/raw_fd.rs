use assert2::{assert, let_assert};
use tokio_seqpacket::UnixSeqpacket;

/// Test a simple send and recv call.
#[tokio::test]
async fn send_recv() {
	let_assert!(Ok((a, b)) = UnixSeqpacket::pair());

	// Round-trip through a raw fd.
	let_assert!(Ok(a) = unsafe { UnixSeqpacket::from_raw_fd(a.into_raw_fd()) });
	let_assert!(Ok(b) = unsafe { UnixSeqpacket::from_raw_fd(b.into_raw_fd()) });

	// Check that the sockets still work.
	assert!(let Ok(12) = a.send(b"Hello world!").await);

	let mut buffer = [0u8; 128];
	assert!(let Ok(12) = b.recv(&mut buffer).await);
	assert!(&buffer[..12] == b"Hello world!");
}
