use assert2::{assert, let_assert};
use tokio_seqpacket::UnixSeqpacket;

/// Test a simple send and recv call.
#[tokio::test]
async fn send_recv() {
	let_assert!(Ok((a, b)) = UnixSeqpacket::pair());
	assert!(let Ok(12) = a.send(b"Hello world!").await);

	let mut buffer = [0u8; 128];
	assert!(let Ok(12) = b.recv(&mut buffer).await);
	assert!(&buffer[..12] == b"Hello world!");
}

/// Test a send and receive call where the send wakes the recv task.
#[test]
fn send_recv_out_of_order() {
	use std::sync::atomic::{AtomicBool, Ordering};

	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let local = tokio::task::LocalSet::new();

	local.block_on(&runtime, async {
		// Atomic bools to verify things happen in the order we want.
		// We're using a local task set to ensure we're single threaded.
		static ABOUT_TO_READ: AtomicBool = AtomicBool::new(false);

		let_assert!(Ok((a, b)) = UnixSeqpacket::pair());

		// Spawning a task shouldn't run anything until the current task awaits something.
		// Still, we use the atomic boolean to double-check that.
		let task = tokio::task::spawn_local(async move {
			assert!(ABOUT_TO_READ.load(Ordering::Relaxed) == true);
			assert!(let Ok(12) = a.send(b"Hello world!").await);
		});

		let mut buffer = [0u8; 128];
		ABOUT_TO_READ.store(true, Ordering::Relaxed);
		assert!(let Ok(12) = b.recv(&mut buffer).await);
		assert!(&buffer[..12] == b"Hello world!");

		assert!(let Ok(()) = task.await);
	});
}

/// Test a simple send_vectored and recv_vectored call.
#[tokio::test]
async fn send_recv_vectored() {
	use std::io::{IoSlice, IoSliceMut};

	let_assert!(Ok((a, b)) = UnixSeqpacket::pair());
	assert!(let Ok(12) = a.send_vectored(&[
		IoSlice::new(b"Hello"),
		IoSlice::new(b" "),
		IoSlice::new(b"world"),
		IoSlice::new(b"!"),
	]).await);

	let mut hello = [0u8; 5];
	let mut space = [0u8; 1];
	let mut world = [0u8; 5];
	let mut punct = [0u8; 1];
	assert!(let Ok(12) = b.recv_vectored(&mut [
		IoSliceMut::new(&mut hello),
		IoSliceMut::new(&mut space),
		IoSliceMut::new(&mut world),
		IoSliceMut::new(&mut punct),
	]).await);

	assert!(&hello == b"Hello");
	assert!(&space == b" ");
	assert!(&world == b"world");
	assert!(&punct == b"!");
}

#[test]
fn echo_loop() {
	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	runtime.block_on(async {
		let_assert!(Ok((client, server)) = UnixSeqpacket::pair());

		let server = tokio::task::spawn(async move {
			let mut buf = vec![0u8; 2048];
			loop {
				println!("waiting for next request");
				let_assert!(Ok(n_received) = server.recv(&mut buf).await);
				println!("received: {}", String::from_utf8_lossy(&buf[..n_received]));
				if n_received == 0 {
					break;
				}
				assert!(let Ok(_) = server.send(&buf[..n_received]).await);
			}
		});
		let client = tokio::task::spawn(async move {
			for i in 0..1024 {
				let message = format!("Hello #{}", i);
				let_assert!(Ok(n_sent) = client.send(message.as_bytes()).await);
				assert!(n_sent == message.len());
				let mut buf = vec![0u8; 1024];
				let_assert!(Ok(n_received) = client.recv(&mut buf).await);
				assert!(message.as_bytes() == &buf[..n_received]);
			}
		});

		let (server_result, client_result) = tokio::join!(server, client);
		assert!(let Ok(()) = server_result);
		assert!(let Ok(()) = client_result);
	});
}

#[test]
fn multiple_waiters() {
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::sync::Arc;

	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	runtime.block_on(async {
		let_assert!(Ok((a, b)) = UnixSeqpacket::pair());
		let a = Arc::new(a);
		let b = Arc::new(b);
		let written = Arc::new(AtomicUsize::new(0));
		let received = Arc::new(AtomicUsize::new(0));

		let read1 = tokio::spawn({
			let a = a.clone();
			let written = written.clone();
			let received = received.clone();
			async move {
				let mut buffer = [0u8; 12];
				assert!(written.load(Ordering::Relaxed) == 0); // Double check that the test will cause recv() to park the current task.
				assert!(let Ok(12) = a.recv(&mut buffer).await);
				assert!(&buffer == b"Hello world!");
				received.fetch_add(1, Ordering::Relaxed);
			}
		});

		let read2 = tokio::spawn({
			let a = a.clone();
			let written = written.clone();
			let received = received.clone();
			async move {
				let mut buffer = [0u8; 12];
				assert!(written.load(Ordering::Relaxed) == 0); // Double check that the test will cause recv() to park the current task.
				assert!(let Ok(12) = a.recv(&mut buffer).await);
				assert!(&buffer == b"Hello world!");
				received.fetch_add(1, Ordering::Relaxed);
			}
		});

		let write = tokio::spawn(async move {
			// Give the readers some time to get parked.
			for _ in 0..10 {
				tokio::task::yield_now().await;
			}
			written.fetch_add(1, Ordering::Relaxed);
			assert!(let Ok(12) = b.send(b"Hello world!").await);
			written.fetch_add(1, Ordering::Relaxed);
			assert!(let Ok(12) = b.send(b"Hello world!").await);
		});

		let (read1, read2, write) = tokio::join!(read1, read2, write);
		assert!(let Ok(()) = read1);
		assert!(let Ok(()) = read2);
		assert!(let Ok(()) = write);
		assert!(received.load(Ordering::Relaxed) == 2);
	});
}
