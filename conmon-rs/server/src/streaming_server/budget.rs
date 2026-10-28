//! The bytes a streaming connection holds for the client. The streaming
//! protocols have no flow control, so a connection bounds what it buffers
//! for all of its streams together and stops reading the client once that
//! is used up. The data gives its share back once it got taken.

use bytes::{Bytes, BytesMut};
use std::{ops::Deref, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The bytes a connection may hold. Clones share them.
#[derive(Clone, Debug)]
pub struct Budget {
    bytes: Arc<Semaphore>,
    max: u32,
}

impl Budget {
    pub fn new(max: u32) -> Self {
        Self {
            bytes: Arc::new(Semaphore::new(max as usize)),
            max,
        }
    }

    /// Hold data within the budget, waits until there is room for it. Data
    /// larger than the whole budget takes all of it, so that every hold
    /// counts.
    pub async fn hold(&self, data: Bytes) -> Held {
        let len = data.len();
        self.hold_as(data, len).await
    }

    /// Hold data which counts as `len` bytes, see [`Self::hold`].
    pub async fn hold_as(&self, data: Bytes, len: usize) -> Held {
        let len = u32::try_from(len).map_or(self.max, |len| len.min(self.max));
        // The semaphore never gets closed.
        let permit = self.bytes.clone().acquire_many_owned(len).await.ok();
        Held { data, permit }
    }

    /// The bytes which are not held right now.
    #[cfg(test)]
    pub fn available(&self) -> usize {
        self.bytes.available_permits()
    }
}

/// Data which counts against a [`Budget`] until it gets dropped.
#[derive(Debug)]
pub struct Held {
    data: Bytes,
    permit: Option<OwnedSemaphorePermit>,
}

impl Held {
    /// The data, its share of the budget is given back.
    pub fn into_bytes(self) -> Bytes {
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
pub struct HeldBuf {
    data: BytesMut,
    permit: Option<OwnedSemaphorePermit>,
}

impl HeldBuf {
    /// What appending `len` bytes counts against the budget, the capacity
    /// the buffer grows by, see [`Self::push`].
    pub fn cost(&self, len: usize) -> usize {
        self.capacity_for(len) - self.data.capacity()
    }

    /// Append data, which has to be held by the same budget as the data
    /// before, as its [`Self::cost`].
    pub fn push(&mut self, held: Held) {
        let capacity = self.capacity_for(held.len());
        if capacity > self.data.capacity() {
            let mut data = BytesMut::with_capacity(capacity);
            data.extend_from_slice(&self.data);
            self.data = data;
        }
        self.data.extend_from_slice(&held.data);
        match (&mut self.permit, held.permit) {
            (Some(permit), Some(other)) => permit.merge(other),
            (permit, other) => *permit = permit.take().or(other),
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
        Self { data, permit: None }
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
