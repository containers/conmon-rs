use assert2::{assert, let_assert};
use tokio_seqpacket::UnixSeqpacket;

/// Test a simple send and recv call.
#[tokio::test]
#[allow(deprecated)] // It may be deprecated, but it should still work.
async fn send_recv() {
	let_assert!(Ok((a, b)) = UnixSeqpacket::pair());

	let (read_a, write_a) = a.split();
	let (read_b, write_b) = b.split();

	assert!(let Ok(_) = write_a.send(b"Hello B!").await);
	assert!(let Ok(_) = write_b.send(b"Hello A!").await);

	let mut buffer = [0u8; 128];

	let_assert!(Ok(len) = read_b.recv(&mut buffer).await);
	assert!(&buffer[..len] == b"Hello B!");

	let_assert!(Ok(len) = read_a.recv(&mut buffer).await);
	assert!(&buffer[..len] == b"Hello A!");
}
