//! Budgets shared by the streaming sessions of one server.

use super::budget::SharedBudget;
use anyhow::{Result, bail};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::warn;

/// Default shared budget for queued SPDY input and port forward buffers.
pub(crate) const DEFAULT_BUFFER_MIB: u16 = 64;
/// Default shared limit of pending and established forwarded connections.
pub(crate) const DEFAULT_FORWARD_CONNECTIONS: u16 = 128;

#[derive(Debug)]
pub(super) struct Resources {
    pub(super) buffer: SharedBudget,
    connections: Arc<Semaphore>,
    connection_limit: usize,
    refused: AtomicUsize,
}

impl Resources {
    pub(super) fn new(buffer_bytes: u32, connections: usize) -> Arc<Self> {
        Arc::new(Self {
            buffer: SharedBudget::new(buffer_bytes),
            connections: Arc::new(Semaphore::new(connections)),
            connection_limit: connections,
            refused: AtomicUsize::new(0),
        })
    }

    /// Reserve before creating sockets, keep through connection teardown.
    /// Refuse instead of queueing more tasks and sockets behind a busy pod.
    pub(super) fn connection(&self) -> Result<OwnedSemaphorePermit> {
        if let Ok(permit) = self.connections.clone().try_acquire_owned() {
            return Ok(permit);
        }
        let refused = self.refused.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        // Report the first saturation and exponential milestones, so a busy
        // client cannot turn the limit into unbounded warning traffic.
        if refused.is_power_of_two() {
            warn!(
                limit = self.connection_limit,
                refused, "Port forward connection limit reached"
            );
        }
        bail!(
            "maximum active port forward connections reached ({})",
            self.connection_limit
        )
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            buffer: SharedBudget::new(u32::from(DEFAULT_BUFFER_MIB) * 1024 * 1024),
            connections: Arc::new(Semaphore::new(usize::from(DEFAULT_FORWARD_CONNECTIONS))),
            connection_limit: usize::from(DEFAULT_FORWARD_CONNECTIONS),
            refused: AtomicUsize::new(0),
        }
    }
}
