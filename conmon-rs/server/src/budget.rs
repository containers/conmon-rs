//! The bytes a streaming connection holds for the client. The streaming
//! protocols have no flow control, so a connection bounds what it buffers
//! for all of its streams together and stops reading the client once that
//! is used up. The data gives its share back once it got taken.

use bytes::Bytes;
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
        let len = u32::try_from(data.len()).map_or(self.max, |len| len.min(self.max));
        // The semaphore never gets closed.
        let permit = self.bytes.clone().acquire_many_owned(len).await.ok();
        Held {
            data,
            _permit: permit,
        }
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
    _permit: Option<OwnedSemaphorePermit>,
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

/// Data outside of any budget.
#[cfg(test)]
impl From<Bytes> for Held {
    fn from(data: Bytes) -> Self {
        Self {
            data,
            _permit: None,
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

    #[tokio::test]
    async fn large_data_takes_the_whole_budget() {
        let budget = Budget::new(4);
        let held = budget.hold(Bytes::from_static(b"0123456789")).await;
        assert_eq!(budget.available(), 0);
        drop(held);
        assert_eq!(budget.available(), 4);
    }
}
