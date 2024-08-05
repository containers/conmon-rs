use assert2::{assert, let_assert};
use tempfile::tempdir;
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};

/// Test that we can accept connections on the listener.
#[tokio::test]
async fn unix_seqpacket_listener() {
	let dir = tempdir().unwrap();
	let path = dir.path().join("listener.sock");

	let server_task = tokio::spawn({
		let_assert!(Ok(mut listener) = UnixSeqpacketListener::bind(&path));
		let_assert!(Ok(local_address) = listener.local_addr());
		assert!(local_address == path);
		async move {
			for _ in 0..2 {
				let_assert!(Ok(peer) = listener.accept().await);
				assert!(let Ok(_) = peer.send(b"Hello!").await);
				let mut buf = [0u8; 128];
				let_assert!(Ok(len) = peer.recv(&mut buf).await);
				assert!(&buf[..len] == b"Goodbye!");
			}
		}
	});

	for _ in 0..2 {
		let_assert!(Ok(peer) = UnixSeqpacket::connect(&path).await);
		let mut buf = [0u8; 128];
		let_assert!(Ok(len) = peer.recv(&mut buf).await);
		assert!(&buf[..len] == b"Hello!");
		assert!(let Ok(_) = peer.send(b"Goodbye!").await);
	}

	assert!(let Ok(()) = server_task.await);
}
