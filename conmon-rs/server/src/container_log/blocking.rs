//! Admission to the blocking pool for log file operations.

use crate::server::Server;
use anyhow::{Context, Result};
use tokio::{sync::Semaphore, task};

/// Leave half of the pool for syncs and lifecycle work. Syncs have their
/// own budget of one quarter of the pool, so even if both groups stall,
/// exit files and socket cleanup can still run.
const MAX_LOG_IO: usize = Server::MAX_BLOCKING_THREADS / 2;
static IO: Semaphore = Semaphore::const_new(MAX_LOG_IO);

/// Wait for admission before submitting work. The blocking task owns the
/// permit, so cancelling its caller cannot admit another operation while
/// the original one is still blocked in the kernel.
pub(super) async fn run<F, T>(work: F) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = IO.acquire().await.context("wait for log file I/O")?;
    task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(Into::into)
}

#[cfg(test)]
pub(crate) fn available() -> usize {
    IO.available_permits()
}
