use std::{collections::VecDeque, fmt::Debug, time::Duration};
// The tokio clock, so that tests can pause and advance it.
use tokio::time::Instant;
use tracing::warn;

/// A map bounded by element age and maximum amount of items. Elements only
/// get inserted and taken, so the oldest one is also the least recently used
/// one, which goes first when the map is full. The map stays small, a linear
/// search is quick enough.
pub struct BoundedHashMap<K, V> {
    /// The elements with the time of their insert, the oldest first.
    entries: VecDeque<(K, Instant, V)>,
    capacity: usize,
    max_duration: Duration,
}

impl<K, V> Debug for BoundedHashMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedHashMap")
            .field("len", &self.entries.len())
            .field("max_duration", &self.max_duration)
            .finish()
    }
}

impl<K, V> BoundedHashMap<K, V>
where
    K: Eq + Debug,
    V: Debug,
{
    /// Insert an element into the map by:
    /// - removing timed-out elements from the oldest end
    /// - removing the oldest element if no space left
    ///
    /// Returns the element which got replaced or removed for space.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let now = Instant::now();
        self.remove_expired(now);

        let evicted = match self.position(&k) {
            Some(pos) => self.entries.remove(pos),
            None if self.entries.len() >= self.capacity => self.entries.pop_front(),
            None => None,
        };
        if evicted.is_some() {
            warn!("Evicted least recently used element due to capacity limit");
        }
        self.entries.push_back((k, now, v));

        evicted.map(|(_, _, val)| val)
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
            warn!("Max duration expired for key: {:?}", k);
            None
        } else {
            Some(value)
        }
    }

    /// Whether the map holds an element for the key which has not expired,
    /// without taking it.
    pub fn contains_key(&self, k: &K) -> bool {
        let now = Instant::now();
        self.entries
            .iter()
            .any(|(key, inserted, _)| key == k && now - *inserted <= self.max_duration)
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
            warn!("Removed {} timed out elements", expired_count);
        }
    }

    /// Get the current number of items in the map
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

const DEFAULT_CAPACITY: usize = 25;

impl<K, V> Default for BoundedHashMap<K, V> {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(DEFAULT_CAPACITY),
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
        let mut sut = BoundedHashMap::default();
        sut.insert(1, "one");
        assert!(sut.contains_key(&1));
        assert!(!sut.contains_key(&2));
        assert_eq!(sut.remove(&1), Some("one"));

        sut.insert(3, "three");
        advance(sut.max_duration + Duration::from_secs(1)).await;
        assert!(!sut.contains_key(&3));
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_hashmap_test() {
        let mut sut = BoundedHashMap {
            entries: VecDeque::new(),
            capacity: 2,
            max_duration: Duration::new(60 * 60, 0),
        };

        assert_eq!(sut.len(), 0);

        // Insert first item should be fine
        assert!(sut.insert(0, 0).is_none());
        assert_eq!(sut.len(), 1);

        // Insert second item should be fine, removal should work as well
        assert!(sut.insert(1, 0).is_none());
        assert_eq!(sut.len(), 2);
        assert!(sut.remove(&1).is_some());
        assert_eq!(sut.len(), 1);
        assert!(sut.insert(1, 0).is_none());

        // Insert third item should evict LRU (item 0)
        let evicted = sut.insert(2, 0);
        assert!(evicted.is_some()); // Should have evicted item 0
        assert_eq!(evicted.unwrap(), 0); // Value of evicted item
        assert_eq!(sut.len(), 2);
        assert!(sut.remove(&0).is_none()); // 0 was evicted
        assert!(sut.remove(&1).is_some());
        assert!(sut.remove(&2).is_some());

        // Re-insert to test LRU ordering
        assert!(sut.insert(1, 0).is_none());
        assert!(sut.insert(2, 0).is_none());
        // Insert 3 should evict 1 (LRU)
        let evicted = sut.insert(3, 0);
        assert!(evicted.is_some()); // Should have evicted item 1
        assert_eq!(evicted.unwrap(), 0); // Value of evicted item
        assert_eq!(sut.len(), 2);
        assert!(sut.remove(&1).is_none()); // 1 was evicted
        assert!(sut.remove(&2).is_some());
        assert!(sut.remove(&3).is_some());

        // Change the max age of the elements, all should be timed out
        sut.max_duration = Duration::from_millis(100);
        assert!(sut.insert(0, 0).is_none());
        advance(Duration::from_millis(200)).await;

        // Insert a new element - should trigger cleanup of expired items
        assert!(sut.insert(1, 0).is_none());
        // Item 0 should be expired and removed
        assert!(sut.remove(&0).is_none());

        // The last element should be also timed out if we wait
        advance(Duration::from_millis(200)).await;
        assert!(sut.remove(&1).is_none());

        // Removing any key removes all expired elements.
        assert!(sut.insert(0, 0).is_none());
        assert!(sut.insert(1, 0).is_none());
        advance(Duration::from_millis(200)).await;
        assert!(sut.remove(&2).is_none());
        assert_eq!(sut.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_elements_are_not_handed_out() {
        let mut sut = BoundedHashMap::default();
        sut.insert(1, "one");
        sut.insert(2, "two");
        advance(sut.max_duration + Duration::from_secs(1)).await;
        sut.insert(3, "three");

        // The expired element is taken, but not handed out, the other expired
        // one goes as well.
        assert_eq!(sut.remove(&1), None);
        assert_eq!(sut.len(), 1);
        assert_eq!(sut.remove(&3), Some("three"));
    }
}
