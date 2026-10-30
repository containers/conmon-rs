//! Small helpers which modules share that do not depend on each other
//! otherwise.

use anyhow::format_err;
use tokio::time::Instant;

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
