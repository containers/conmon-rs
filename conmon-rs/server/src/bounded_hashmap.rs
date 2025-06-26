use std::{
    collections::HashMap,
    fmt::Debug,
    hash::Hash,
    time::{Duration, Instant},
};
use tracing::warn;

#[derive(Debug)]
/// A HashMap bounded by element age and maximum amount of items
pub struct BoundedHashMap<K, V> {
    map: HashMap<K, (Instant, V)>,
    max_duration: Duration,
    max_items: usize,
}

impl<K, V> BoundedHashMap<K, V>
where
    K: Eq + Hash + Clone + Debug,
    V: Debug,
{
    /// Insert an element into the hashmap by:
    /// - removing timed-out elements
    /// - removing the oldest element if no space left
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let now = Instant::now();

        // Remove timed-out items
        let old_len = self.map.len();
        self.map
            .retain(|_, (inserted, _)| now - *inserted <= self.max_duration);
        if old_len < self.map.len() {
            warn!("Removed {} timed out elements", self.map.len() - old_len)
        }

        // Remove the oldest entry if still not enough space left
        if self.map.len() >= self.max_items {
            let mut key_to_remove = k.clone();

            let mut oldest = now;
            for (key, (inserted, _)) in self.map.iter() {
                if *inserted < oldest {
                    oldest = *inserted;
                    key_to_remove = key.clone();
                }
            }

            warn!("Removing oldest key: {:?}", key_to_remove);
            self.map.remove(&key_to_remove);
        }

        self.map.insert(k, (Instant::now(), v)).map(|v| v.1)
    }

    /// Remove an element from the hashmap and return it if the element has not expired.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let now = Instant::now();

        if let Some((key, (inserted, value))) = self.map.remove_entry(k) {
            if now - inserted > self.max_duration {
                warn!("Max duration expired for key: {:?}", key);
                None
            } else {
                Some(value)
            }
        } else {
            None
        }
    }
}

impl<K, V> Default for BoundedHashMap<K, V> {
    fn default() -> Self {
        Self {
            map: HashMap::with_capacity(0),
            max_duration: Duration::new(60 * 60, 0), // 1 hour
            max_items: 1000,
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
            max_items: 2,
            ..Default::default()
        };

        assert_eq!(sut.map.len(), 0);

        // Insert first item should be fine
        assert!(sut.insert(0, 0).is_none());
        assert_eq!(sut.map.len(), 1);

        // Insert second item should be fine, removal should work as well
        assert!(sut.insert(1, 0).is_none());
        assert_eq!(sut.map.len(), 2);
        assert!(sut.remove(&1).is_some());
        assert_eq!(sut.map.len(), 1);
        assert!(sut.insert(1, 0).is_none());

        // Insert third item should be fine, but remove oldest
        assert!(sut.insert(2, 0).is_none());
        assert_eq!(sut.map.len(), 2);
        assert!(!sut.map.contains_key(&0));
        assert!(sut.map.contains_key(&1));
        assert!(sut.map.contains_key(&2));

        // Insert another item should be fine, but remove oldest
        assert!(sut.insert(3, 0).is_none());
        assert_eq!(sut.map.len(), 2);
        assert!(!sut.map.contains_key(&1));
        assert!(sut.map.contains_key(&2));
        assert!(sut.map.contains_key(&3));

        // Change the max age of the elements, all should be timed out
        sut.max_duration = Duration::from_millis(100);
        sleep(Duration::from_millis(200));
        assert!(sut.insert(0, 0).is_none());
        assert!(!sut.map.contains_key(&1));
        assert!(!sut.map.contains_key(&2));
        assert!(!sut.map.contains_key(&3));

        // The last element should be also timed out if we wait
        sleep(Duration::from_millis(200));
        assert!(sut.remove(&0).is_none());
    }
}
