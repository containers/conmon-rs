//! A pure-Rust client library to work with systemd.
//!
//! It provides support to interact with systemd components available
//! on modern Linux systems. This crate is entirely implemented
//! in Rust, and does not require an external libsystemd dynamic library.
//!
//! ```rust
//! use libsystemd::daemon::{self, NotifyState};
//!
//! fn notify_ready() -> bool {
//!     if !daemon::booted() {
//!         println!("Not running systemd, early exit.");
//!         return false;
//!     };
//!
//!     let sent = daemon::notify(true, &[NotifyState::Ready]).expect("notify failed");
//!     if !sent {
//!         println!("Notification not sent!");
//!     };
//!     sent
//! }
//! ```

/// Interfaces for socket-activated services.
pub mod activation;
/// Helpers for securely passing potentially sensitive data to services.
pub mod credentials;
/// Interfaces for systemd-aware daemons.
pub mod daemon;
/// Error handling.
pub mod errors;
/// APIs for processing 128-bits IDs.
pub mod id128;
/// Helpers for logging to `systemd-journald`.
pub mod logging;
pub mod sysusers;
/// Helpers for working with systemd units.
pub mod unit;
