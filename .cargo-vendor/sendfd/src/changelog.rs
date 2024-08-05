//! Project changelog

/// Release 0.4.1
///
/// * Asynchronous socket support has been fixed to register interest with the runtime. In absence
/// of this fix it was possible for an application to busy loop waiting for changes.
pub mod r0_4_1 {}

/// Release 0.4.2
///
/// * Implemented [SendWithFd](crate::SendWithFd) for [tokio::net::unix::WriteHalf]
/// * Implemented [RecvWithFd](crate::RecvWithFd) for [tokio::net::unix::ReadHalf]
pub mod r0_4_2 {}

/// Release 0.4.0
///
/// * tokio 0.2 and tokio 0.3 support has been replaced by support for tokio 1.0.
/// * 1.53.0 is now the minimum supported version of the `rustc` toolchain.
pub mod r0_4_0 {}

/// Release 0.3.3
///
/// * Compatibility with tokio::net::UnixStream and tokio::net::UnixDatagram for tokio 0.2 and 0.3.
pub mod r0_3_3 {}

/// Release 0.3.2
///
/// * Compatibility with musl.
pub mod r0_3_2 {}

/// Release 0.3.1
///
/// * Compatibility with macOS and BSDs.
pub mod r0_3_1 {}

/// Release 0.3.0
///
/// * Removed the `Receivable` trait, because it is difficult to write meaningful code with `<T as
/// Receivable>` for `T ≠ RawFd`.
/// * Removed the `Sendable` trait. While the generalisation here is sound and is beneficial, the
/// inconsistency between ability to send any sendable and then only being able to receive a
/// `RawFd` was deemed to be not worth it.
pub mod r0_3_0 {}

/// Release 0.2.1
///
/// Removed an accidentally publicly exported internal function.
///
/// 0.2.0 has been yanked.
pub mod r0_2_1 {}

/// Release 0.2.0
///
/// Pure-Rust reimplementation of the crate.
pub mod r0_2_0 {}
