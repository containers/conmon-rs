//! Unix seqpacket sockets for [tokio](https://docs.rs/tokio).
//!
//! Seqpacket sockets combine a number of useful properties:
//! * They are connection oriented.
//! * They guarantee in-order message delivery.
//! * They provide datagrams with well-defined semantics for passing along file descriptors.
//!
//! These properties make seqpacket sockets very well suited for local servers that need to pass file-descriptors around with their clients.
//!
//! You can create a [`UnixSeqpacketListener`] to start accepting connections,
//! or create a [`UnixSeqpacket`] to connect to a listening socket.
//! You can also create a pair of connected sockets with [`UnixSeqpacket::pair()`].
//!
//! # Passing file descriptors and other ancillary data.
//!
//! You can use [`send_vectored_with_ancillary`][UnixSeqpacket::send_vectored_with_ancillary] and [`recv_vectored_with_ancillary`][UnixSeqpacket::recv_vectored_with_ancillary]
//! to send and receive ancillary data.
//! This can be used to pass file descriptors and unix credentials over sockets.
//!
//! # `&self` versus `&mut self`
//!
//! Seqpacket sockets have well-defined semantics when sending or receiving on the same socket from different threads.
//! Although the order is not guaranteed in that scenario, each datagram will be delivered intact.
//! Since tokio 0.3, it is also possible for multiple tasks to await the same file descriptor.
//! As such, all I/O functions now take `&self` instead of `&mut self`,
//! and the `split()` API has been deprecated.
//!
//! # Example
//! ```no_run
//! # async fn foo() -> Result<(), Box<dyn std::error::Error>> {
//! use tokio_seqpacket::UnixSeqpacket;
//!
//! let mut socket = UnixSeqpacket::connect("/run/foo.sock").await?;
//! socket.send(b"Hello!").await?;
//!
//! let mut buffer = [0u8; 128];
//! let len = socket.recv(&mut buffer).await?;
//! println!("{}", String::from_utf8_lossy(&buffer[..len]));
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

macro_rules! ready {
	($e:expr) => {
		match $e {
			Poll::Pending => return Poll::Pending,
			Poll::Ready(x) => x,
		}
	};
}

pub mod ancillary;
pub mod borrow_fd;
mod listener;
mod socket;
mod sys;
mod ucred;

pub use listener::UnixSeqpacketListener;
pub use socket::UnixSeqpacket;
pub use ucred::UCred;

#[doc(hidden)]
#[deprecated(
	since = "0.4.0",
	note = "all I/O functions now take a shared reference to self, so splitting is no longer necessary"
)]
pub type ReadHalf<'a> = &'a UnixSeqpacket;

#[doc(hidden)]
#[deprecated(
	since = "0.4.0",
	note = "all I/O functions now take a shared reference to self, so splitting is no longer necessary"
)]
pub type WriteHalf<'a> = &'a UnixSeqpacket;
