use lru::LruCache;
use std::{
    fmt::Debug,
    hash::Hash,
    num::NonZeroUsize,
    time::{Duration, Instant},
};
use tracing::warn;

/// A HashMap bounded by element age and maximum amount of items
/// Uses LRU eviction for O(1) performance when at capacity
pub struct BoundedHashMap<K, V> {
    cache: LruCache<K, (Instant, V)>,
    max_duration: Duration,
}

impl<K, V> Debug for BoundedHashMap<K, V>
where
    K: Eq + Hash,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedHashMap")
            .field("len", &self.cache.len())
            .field("max_duration", &self.max_duration)
            .finish()
    }
}

impl<K, V> BoundedHashMap<K, V>
where
    K: Eq + Hash + Clone + Debug,
    V: Debug,
{
    /// Insert an element into the hashmap by:
    /// - removing timed-out elements
    /// - removing the least recently used element if no space left (automatic via LRU)
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let now = Instant::now();

        // Remove timed-out items by iterating and collecting expired keys
        let mut expired_keys = Vec::new();
        for (key, (inserted, _)) in self.cache.iter() {
            if now - *inserted > self.max_duration {
                expired_keys.push(key.clone());
            }
        }

        let expired_count = expired_keys.len();
        for key in expired_keys {
            self.cache.pop(&key);
        }

        if expired_count > 0 {
            warn!("Removed {} timed out elements", expired_count);
        }

        // LRU cache automatically evicts the least recently used item when at capacity
        // This is O(1) instead of the previous O(n) scan
        let evicted = self.cache.push(k, (Instant::now(), v));
        if evicted.is_some() {
            warn!("Evicted least recently used element due to capacity limit");
        }

        // Extract the actual value from the evicted (K, (Instant, V)) tuple
        evicted.map(|(_, (_, val))| val)
    }

    /// Remove an element from the hashmap and return it if the element has not expired.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let now = Instant::now();

        if let Some((inserted, value)) = self.cache.pop(k) {
            if now - inserted > self.max_duration {
                warn!("Max duration expired for key: {:?}", k);
                None
            } else {
                Some(value)
            }
        } else {
            None
        }
    }

    /// Get the current number of items in the cache
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Check if the cache is empty
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

impl<K, V> Default for BoundedHashMap<K, V>
where
    K: Eq + Hash + Clone + Debug,
    V: Debug,
{
    fn default() -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(25).unwrap()),
            max_duration: Duration::new(60 * 60, 0), // 1 hour
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn bounded_hashmap_test() {
        let mut sut = BoundedHashMap {
            cache: LruCache::new(NonZeroUsize::new(2).unwrap()),
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
        sleep(Duration::from_millis(200));

        // Insert a new element - should trigger cleanup of expired items
        assert!(sut.insert(1, 0).is_none());
        // Item 0 should be expired and removed
        assert!(sut.remove(&0).is_none());

        // The last element should be also timed out if we wait
        sleep(Duration::from_millis(200));
        assert!(sut.remove(&1).is_none());
    }
}
