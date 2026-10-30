//! The tokens of the streaming sessions which wait for their clients, bounded
//! in age and number.

use std::{collections::VecDeque, fmt::Debug, time::Duration};
// The tokio clock, so that tests can pause and advance it.
use tokio::time::Instant;
use tracing::debug;

/// A map bounded by element age and maximum amount of items. Elements only
/// get inserted and taken. A full map takes no more elements until one of
/// them got taken or expired, a valid element never gets dropped for a new
/// one. Clients take their element right after it got inserted, the map
/// holds few of them and a linear search is quick enough.
pub struct BoundedMap<K, V> {
    /// The elements with the time of their insert, the oldest first.
    entries: VecDeque<(K, Instant, V)>,
    capacity: usize,
    max_duration: Duration,
}

impl<K, V> Debug for BoundedMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedMap")
            .field("len", &self.entries.len())
            .field("max_duration", &self.max_duration)
            .finish()
    }
}

impl<K, V> BoundedMap<K, V>
where
    K: Eq + Debug,
{
    /// Insert an element into the map, after removing the timed-out ones.
    /// Returns the new one back if the map is full. The keys are random
    /// tokens, which do not repeat, so the map does not look for an element
    /// with the same key.
    pub fn insert(&mut self, k: K, v: V) -> Result<(), V> {
        let now = Instant::now();
        self.remove_expired(now);

        if self.entries.len() >= self.capacity {
            return Err(v);
        }
        self.entries.push_back((k, now, v));
        Ok(())
    }

    /// Remove an element from the map and return it if the element has not
    /// expired. The expired ones get removed as well, so that they do not
    /// linger until the next insert, they may hold sockets.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let now = Instant::now();
        // Taken before the other expired ones go, which tells about a client
        // which came too late.
        let entry = self.position(k).and_then(|pos| self.entries.remove(pos));
        self.remove_expired(now);

        let (_, inserted, value) = entry?;
        if now - inserted > self.max_duration {
            debug!("Max duration expired for key: {:?}", k);
            None
        } else {
            Some(value)
        }
    }

    /// Whether the map holds an element for the key which has not expired,
    /// without taking it.
    pub fn contains_key(&self, k: &K) -> bool {
        self.get(k).is_some()
    }

    /// The element for the key if it has not expired, without taking it.
    pub fn get(&self, k: &K) -> Option<&V> {
        let now = Instant::now();
        self.entries
            .iter()
            .find(|(key, inserted, _)| key == k && now - *inserted <= self.max_duration)
            .map(|(_, _, value)| value)
    }

    /// Remove the expired elements, which may hold resources until then.
    pub fn sweep(&mut self) {
        self.remove_expired(Instant::now());
    }

    fn position(&self, k: &K) -> Option<usize> {
        self.entries.iter().position(|(key, _, _)| key == k)
    }

    /// Remove expired items from the oldest end. If the oldest one is not
    /// expired, no entries are expired, so this can stop early.
    fn remove_expired(&mut self, now: Instant) {
        let mut expired_count = 0;
        while let Some((_, inserted, _)) = self.entries.front() {
            if now - *inserted > self.max_duration {
                self.entries.pop_front();
                expired_count += 1;
            } else {
                break;
            }
        }

        if expired_count > 0 {
            debug!("Removed {} timed out elements", expired_count);
        }
    }

    /// Whether the map holds no elements, also no expired ones.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the current number of items in the map
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The most elements of a map, like the most streaming requests in flight
/// of the kubelet and CRI-O. An element only holds what its client needs to
/// start, a low limit would let clients which do not come fail the others
/// until the elements expire.
pub const DEFAULT_CAPACITY: usize = 1000;

impl<K, V> Default for BoundedMap<K, V> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: DEFAULT_CAPACITY,
            // A client connects right after it got the URL, the kubelet
            // keeps its streaming requests for a minute as well.
            max_duration: Duration::from_secs(60),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::advance;

    #[tokio::test(start_paused = true)]
    async fn contains_key_leaves_the_element() {
        let mut sut = BoundedMap::default();
        assert_eq!(sut.insert(1, "one"), Ok(()));
        assert!(sut.contains_key(&1));
        assert!(!sut.contains_key(&2));
        assert_eq!(sut.remove(&1), Some("one"));

        assert_eq!(sut.insert(3, "three"), Ok(()));
        advance(sut.max_duration + Duration::from_secs(1)).await;
        assert!(!sut.contains_key(&3));
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_map_test() {
        let mut sut = BoundedMap {
            entries: VecDeque::new(),
            capacity: 2,
            max_duration: Duration::new(60 * 60, 0),
        };

        assert_eq!(sut.len(), 0);

        // Insert first item should be fine
        assert_eq!(sut.insert(0, 0), Ok(()));
        assert_eq!(sut.len(), 1);

        // Insert second item should be fine, removal should work as well
        assert_eq!(sut.insert(1, 0), Ok(()));
        assert_eq!(sut.len(), 2);
        assert!(sut.remove(&1).is_some());
        assert_eq!(sut.len(), 1);
        assert_eq!(sut.insert(1, 0), Ok(()));

        // A full map takes no third item, the valid ones stay.
        assert_eq!(sut.insert(2, 2), Err(2));
        assert_eq!(sut.len(), 2);
        assert!(sut.remove(&2).is_none());

        assert!(sut.remove(&0).is_some());
        assert!(sut.remove(&1).is_some());

        // Change the max age of the elements, all should be timed out
        sut.max_duration = Duration::from_millis(100);
        assert_eq!(sut.insert(0, 0), Ok(()));
        assert_eq!(sut.insert(1, 0), Ok(()));
        advance(Duration::from_millis(200)).await;

        // A full map of expired items takes a new one.
        assert_eq!(sut.insert(2, 0), Ok(()));
        assert_eq!(sut.len(), 1);
        assert!(sut.remove(&0).is_none());

        // The last element should be also timed out if we wait
        advance(Duration::from_millis(200)).await;
        assert!(sut.remove(&2).is_none());

        // Removing any key removes all expired elements.
        assert_eq!(sut.insert(0, 0), Ok(()));
        assert_eq!(sut.insert(1, 0), Ok(()));
        advance(Duration::from_millis(200)).await;
        assert!(sut.remove(&2).is_none());
        assert_eq!(sut.len(), 0);

        // So does a sweep.
        assert_eq!(sut.insert(0, 0), Ok(()));
        advance(Duration::from_millis(200)).await;
        assert!(!sut.is_empty());
        sut.sweep();
        assert!(sut.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn expired_elements_are_not_handed_out() {
        let mut sut = BoundedMap::default();
        assert_eq!(sut.insert(1, "one"), Ok(()));
        assert_eq!(sut.insert(2, "two"), Ok(()));
        advance(sut.max_duration + Duration::from_secs(1)).await;
        assert_eq!(sut.insert(3, "three"), Ok(()));

        // The expired element is taken, but not handed out, the other expired
        // one goes as well.
        assert_eq!(sut.remove(&1), None);
        assert_eq!(sut.len(), 1);
        assert_eq!(sut.remove(&3), Some("three"));
    }
}
