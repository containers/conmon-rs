//! Small helpers which modules share that do not depend on each other
//! otherwise.

use anyhow::format_err;
use std::cell::RefCell;
use tokio::time::Instant;

thread_local! {
    /// See [`with_scratch`].
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` on a buffer of `len` bytes which the thread keeps for all reads
/// which copy what they got out of it right away. The read loops of all
/// containers and attach clients and the runtime calls share it, instead of
/// keeping a buffer each while they wait. It grows to the largest `len`
/// asked for and holds what the last use left in it.
pub(crate) fn with_scratch<R>(len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    SCRATCH.with(|scratch| match scratch.try_borrow_mut() {
        Ok(mut scratch) => {
            if scratch.len() < len {
                scratch.resize(len, 0);
            }
            f(&mut scratch[..len])
        }
        // Only if `f` used it again, which none of the reads does.
        Err(_) => f(&mut vec![0; len]),
    })
}

/// Sleep until the deadline, forever without one.
pub(crate) async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Append the stderr of a runtime which failed to its error, unless it wrote
/// nothing. It usually holds the reason.
pub(crate) fn with_stderr(error: anyhow::Error, stderr: &[u8]) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        error
    } else {
        format_err!("{error:#}: {stderr}")
    }
}
