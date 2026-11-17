//! The bytes a streaming connection holds for the client. The streaming
//! protocols have no flow control, so a connection bounds what it buffers
//! for all of its streams together and stops reading the client once that
//! is used up. The data gives its share back once it got taken.

use bytes::{Bytes, BytesMut};
use std::{
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{self, Duration, error::Elapsed},
};
use tracing::warn;

/// Actual buffered bytes across sessions, in addition to their local limits.
#[derive(Clone, Debug)]
pub(super) struct SharedBudget(Arc<SharedBytes>);

#[derive(Debug)]
struct SharedBytes {
    bytes: Arc<Semaphore>,
    max: u32,
    waits: AtomicUsize,
}

impl SharedBudget {
    pub(super) fn new(max: u32) -> Self {
        Self(Arc::new(SharedBytes {
            bytes: Arc::new(Semaphore::new(max as usize)),
            max,
            waits: AtomicUsize::new(0),
        }))
    }

    async fn hold(&self, len: u32) -> OwnedSemaphorePermit {
        if let Ok(permit) = self.0.bytes.clone().try_acquire_many_owned(len) {
            return permit;
        }
        let waits = self.0.waits.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if waits.is_power_of_two() {
            warn!(
                limit_bytes = self.0.max,
                waits, "Streaming buffer budget reached, applying backpressure"
            );
        }
        self.0
            .bytes
            .clone()
            .acquire_many_owned(len)
            .await
            .expect("budget stays open")
    }

    #[cfg(test)]
    pub(super) fn available(&self) -> usize {
        self.0.bytes.available_permits()
    }
}

/// The bytes a connection may hold. Clones share them.
#[derive(Clone, Debug)]
pub(super) struct Budget {
    bytes: Arc<Semaphore>,
    max: u32,
    shared: Option<SharedBudget>,
}

impl Budget {
    /// A budget of `max` bytes.
    pub(super) fn new(max: u32) -> Self {
        Self {
            bytes: Arc::new(Semaphore::new(max as usize)),
            max,
            shared: None,
        }
    }

    /// A per-connection budget whose bytes also count against the server.
    pub(super) fn with_shared(max: u32, shared: SharedBudget) -> Self {
        Self {
            shared: Some(shared),
            ..Self::new(max)
        }
    }

    /// Hold data within the budget, waits until there is room for it. Data
    /// larger than the whole budget takes all of it, so that every hold
    /// counts.
    pub(super) async fn hold(&self, data: Bytes) -> Held {
        let len = data.len();
        self.hold_as(data, len).await
    }

    /// Hold data which counts as `len` bytes, see [`Self::hold`].
    pub(super) async fn hold_as(&self, data: Bytes, len: usize) -> Held {
        let mut held = self.hold_local(data, len).await;
        if let Some(shared) = &self.shared {
            held.shared = Some(shared.hold(held.local_len()).await);
        }
        held
    }

    /// Bound only contention for the server's shared bytes. Local input
    /// backpressure keeps its existing wait, governed by the session's idle
    /// timeout, which may be disabled for a slow exec or attach consumer.
    pub(super) async fn hold_with_shared_timeout(
        &self,
        data: Bytes,
        timeout: Duration,
    ) -> Result<Held, Elapsed> {
        let len = data.len();
        let mut held = self.hold_local(data, len).await;
        if let Some(shared) = &self.shared {
            held.shared = Some(time::timeout(timeout, shared.hold(held.local_len())).await?);
        }
        Ok(held)
    }

    async fn hold_local(&self, data: Bytes, len: usize) -> Held {
        let len = u32::try_from(len).map_or(self.max, |len| len.min(self.max));
        // The semaphore never gets closed.
        let permit = self.bytes.clone().acquire_many_owned(len).await.ok();
        Held {
            data,
            permit,
            shared: None,
        }
    }

    /// Move bytes between budgets of the same server without charging them
    /// twice. Only extra capacity needs another shared reservation. Keeping
    /// the source intact until every await finished makes cancellation safe
    /// while a connection gets established, see `PortForward::drain_until`.
    pub(super) async fn transfer(&self, held: &mut Held, len: usize) -> Held {
        let len = u32::try_from(len).map_or(self.max, |len| len.min(self.max));
        let permit = self.bytes.clone().acquire_many_owned(len).await.ok();
        let previous = held
            .shared
            .as_ref()
            .map_or(0, OwnedSemaphorePermit::num_permits);
        let extra = match &self.shared {
            Some(shared) if len as usize > previous => Some(
                shared
                    .hold(len - u32::try_from(previous).expect("permit count fits budget"))
                    .await,
            ),
            _ => None,
        };
        let mut shared = held.shared.take();
        if let Some(permit) = &mut shared {
            // Shrinking releases the unused capacity immediately.
            if previous > len as usize {
                drop(permit.split(previous - len as usize));
            }
            if let Some(extra) = extra {
                permit.merge(extra);
            }
        } else {
            shared = extra;
        }
        Held {
            data: held.data.clone(),
            permit,
            shared,
        }
    }

    /// The bytes which are not held right now.
    #[cfg(test)]
    pub(super) fn available(&self) -> usize {
        self.bytes.available_permits()
    }
}

/// Data which counts against a [`Budget`] until it gets dropped.
#[derive(Debug)]
pub(super) struct Held {
    data: Bytes,
    permit: Option<OwnedSemaphorePermit>,
    shared: Option<OwnedSemaphorePermit>,
}

impl Held {
    fn local_len(&self) -> u32 {
        self.permit.as_ref().map_or(0, |permit| {
            u32::try_from(permit.num_permits()).expect("permit count fits budget")
        })
    }

    /// The data, its share of the budget is given back.
    pub(super) fn into_bytes(self) -> Bytes {
        self.data
    }
}

impl Deref for Held {
    type Target = Bytes;

    fn deref(&self) -> &Bytes {
        &self.data
    }
}

/// Held data of one [`Budget`] in a single buffer, which counts against the
/// budget with all of its capacity until it gets dropped. Unlike a queue of
/// [`Held`], small data does not take an allocation of its own, which the
/// budget does not count.
#[derive(Debug, Default)]
pub(super) struct HeldBuf {
    data: BytesMut,
    permit: Option<OwnedSemaphorePermit>,
    shared: Option<OwnedSemaphorePermit>,
}

impl HeldBuf {
    /// What appending `len` bytes counts against the budget, the capacity
    /// the buffer grows by, see [`Self::push`].
    pub(super) fn cost(&self, len: usize) -> usize {
        self.capacity_for(len) - self.data.capacity()
    }

    /// Append data, which has to be held by the same budget as the data
    /// before, as its [`Self::cost`].
    pub(super) fn push(&mut self, held: Held) {
        let capacity = self.capacity_for(held.len());
        if capacity > self.data.capacity() {
            let mut data = BytesMut::with_capacity(capacity);
            data.extend_from_slice(&self.data);
            self.data = data;
        }
        self.data.extend_from_slice(&held.data);
        for (permit, other) in [
            (&mut self.permit, held.permit),
            (&mut self.shared, held.shared),
        ] {
            match (permit, other) {
                (Some(permit), Some(other)) => permit.merge(other),
                (permit, other) => *permit = permit.take().or(other),
            }
        }
    }

    /// The capacity for `len` more bytes. It at least doubles once the
    /// buffer is full, so that the data does not get copied for each push.
    fn capacity_for(&self, len: usize) -> usize {
        let (len, capacity) = (self.data.len() + len, self.data.capacity());
        if len <= capacity {
            capacity
        } else {
            len.max(2 * capacity)
        }
    }
}

impl Deref for HeldBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.data
    }
}

/// Data outside of any budget.
#[cfg(test)]
impl From<Bytes> for Held {
    fn from(data: Bytes) -> Self {
        Self {
            data,
            permit: None,
            shared: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{self, Duration};

    #[tokio::test(start_paused = true)]
    async fn data_waits_for_room() {
        let budget = Budget::new(10);
        let first = budget.hold(Bytes::from_static(b"0123456")).await;
        assert_eq!(budget.available(), 3);
        assert!(
            time::timeout(
                Duration::from_secs(1),
                budget.hold(Bytes::from_static(b"0123"))
            )
            .await
            .is_err()
        );
        assert_eq!(first.into_bytes(), Bytes::from_static(b"0123456"));
        let second = budget.hold(Bytes::from_static(b"0123")).await;
        assert_eq!(&second[..], b"0123");
        assert_eq!(budget.available(), 6);
        drop(second);
        assert_eq!(budget.available(), 10);
    }

    async fn push(budget: &Budget, buf: &mut HeldBuf, data: &'static [u8]) {
        let cost = buf.cost(data.len());
        buf.push(budget.hold_as(Bytes::from_static(data), cost).await);
    }

    #[tokio::test]
    async fn held_data_gets_collected() {
        let budget = Budget::new(10);
        let mut buf = HeldBuf::default();
        assert!(buf.is_empty());
        for data in [&b"012"[..], b"", b"3456"] {
            push(&budget, &mut buf, data).await;
        }
        assert_eq!(&buf[..], b"0123456");
        assert_eq!(budget.available(), 10 - buf.data.capacity());
        drop(buf);
        assert_eq!(budget.available(), 10);
    }

    #[tokio::test]
    async fn held_buffers_count_their_capacity() {
        let budget = Budget::new(1 << 20);
        let mut buf = HeldBuf::default();
        for _ in 0..1000 {
            push(&budget, &mut buf, b"0123456789").await;
            assert_eq!(budget.available(), (1 << 20) - buf.data.capacity());
        }
        assert_eq!(buf.len(), 10_000);
        // It grew by doubling, without copying the data for each push.
        assert!(buf.data.capacity() < 20_000, "{}", buf.data.capacity());
        drop(buf);
        assert_eq!(budget.available(), 1 << 20);
    }

    #[tokio::test]
    async fn large_data_takes_the_whole_budget() {
        let budget = Budget::new(4);
        let held = budget.hold(Bytes::from_static(b"0123456789")).await;
        assert_eq!(budget.available(), 0);
        drop(held);
        assert_eq!(budget.available(), 4);
    }
}

#[cfg(test)]
mod shared_tests {
    use super::*;
    use tokio::time::{self, Duration};

    #[tokio::test(start_paused = true)]
    async fn sessions_share_a_budget_and_cancelled_waits_release_local_bytes() {
        let shared = SharedBudget::new(8);
        let first = Budget::with_shared(8, shared.clone());
        let second = Budget::with_shared(8, shared.clone());
        let held = first.hold(Bytes::from_static(b"123456")).await;
        assert_eq!(shared.available(), 2);
        assert!(
            time::timeout(
                Duration::from_secs(1),
                second.hold(Bytes::from_static(b"abc"))
            )
            .await
            .is_err()
        );
        assert_eq!(second.available(), 8);
        assert_eq!(shared.available(), 2);
        drop(held);
        let held = second.hold(Bytes::from_static(b"abc")).await;
        assert_eq!(shared.available(), 5);
        drop(held);
        assert_eq!(shared.available(), 8);
    }

    #[tokio::test(start_paused = true)]
    async fn transfers_reuse_shared_bytes_even_when_full() {
        let shared = SharedBudget::new(4);
        let input = Budget::with_shared(4, shared.clone());
        let connecting = Budget::with_shared(4, shared.clone());
        let mut held = input.hold(Bytes::from_static(b"1234")).await;
        let moved = time::timeout(Duration::from_secs(1), connecting.transfer(&mut held, 4))
            .await
            .unwrap();
        drop(held);
        assert_eq!(input.available(), 4);
        assert_eq!(shared.available(), 0);
        let mut buf = HeldBuf::default();
        buf.push(moved);
        assert_eq!(&*buf, b"1234");
        drop(buf);
        assert_eq!(shared.available(), 4);
        assert_eq!(connecting.available(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_growth_preserves_the_source_and_releases_reservations() {
        let shared = SharedBudget::new(8);
        let input = Budget::with_shared(8, shared.clone());
        let connecting = Budget::with_shared(8, shared.clone());
        let mut held = input.hold(Bytes::from_static(b"abc")).await;
        let other = input.hold(Bytes::from_static(b"12345")).await;
        assert!(
            time::timeout(Duration::from_secs(1), connecting.transfer(&mut held, 4))
                .await
                .is_err()
        );
        assert_eq!(&**held, b"abc");
        assert_eq!(connecting.available(), 8);
        assert_eq!(shared.available(), 0);
        drop(other);
        let moved = connecting.transfer(&mut held, 4).await;
        drop(held);
        assert_eq!(shared.available(), 4);
        drop(moved);
        assert_eq!(shared.available(), 8);
    }

    #[tokio::test]
    async fn buffer_capacity_transfers_release_surplus_input_reservations() {
        let shared = SharedBudget::new(100);
        let input = Budget::with_shared(100, shared.clone());
        let connecting = Budget::with_shared(100, shared.clone());
        let mut buf = HeldBuf::default();
        for data in [&b"abc"[..], b"de", b"f", b"ghi"] {
            let mut held = input.hold(Bytes::copy_from_slice(data)).await;
            let moved = connecting.transfer(&mut held, buf.cost(data.len())).await;
            drop(held);
            buf.push(moved);
            assert_eq!(shared.available(), 100 - buf.data.capacity());
        }
        assert_eq!(&*buf, b"abcdefghi");
        drop(buf);
        assert_eq!(shared.available(), 100);
    }
}
