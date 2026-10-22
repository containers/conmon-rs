use std::{collections::VecDeque, fmt::Debug, time::Duration};
// The tokio clock, so that tests can pause and advance it.
use tokio::time::Instant;
use tracing::debug;

/// A map bounded by element age and maximum amount of items. Elements only
/// get inserted and taken. A full map takes no more elements until one of
/// them got taken or expired, a valid element never gets dropped for a new
/// one. Clients take their element right after it got inserted, the map
/// holds few of them and a linear search is quick enough.
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
    /// Insert an element into the map, after removing the timed-out ones.
    /// Returns the element which got replaced, or the new one back if the map
    /// is full.
    pub fn insert(&mut self, k: K, v: V) -> Result<Option<V>, V> {
        let now = Instant::now();
        self.remove_expired(now);

        let replaced = match self.position(&k) {
            Some(pos) => self.entries.remove(pos),
            None if self.entries.len() >= self.capacity => return Err(v),
            None => None,
        };
        self.entries.push_back((k, now, v));

        Ok(replaced.map(|(_, _, val)| val))
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
        let now = Instant::now();
        self.entries
            .iter()
            .any(|(key, inserted, _)| key == k && now - *inserted <= self.max_duration)
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

impl<K, V> Default for BoundedHashMap<K, V> {
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
        let mut sut = BoundedHashMap::default();
        assert_eq!(sut.insert(1, "one"), Ok(None));
        assert!(sut.contains_key(&1));
        assert!(!sut.contains_key(&2));
        assert_eq!(sut.remove(&1), Some("one"));

        assert_eq!(sut.insert(3, "three"), Ok(None));
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
        assert_eq!(sut.insert(0, 0), Ok(None));
        assert_eq!(sut.len(), 1);

        // Insert second item should be fine, removal should work as well
        assert_eq!(sut.insert(1, 0), Ok(None));
        assert_eq!(sut.len(), 2);
        assert!(sut.remove(&1).is_some());
        assert_eq!(sut.len(), 1);
        assert_eq!(sut.insert(1, 0), Ok(None));

        // A full map takes no third item, the valid ones stay.
        assert_eq!(sut.insert(2, 2), Err(2));
        assert_eq!(sut.len(), 2);
        assert!(sut.remove(&2).is_none());

        // An item with the same key replaces the old one.
        assert_eq!(sut.insert(1, 1), Ok(Some(0)));
        assert_eq!(sut.len(), 2);
        assert_eq!(sut.remove(&1), Some(1));
        assert_eq!(sut.insert(2, 0), Ok(None));
        assert!(sut.remove(&0).is_some());
        assert!(sut.remove(&2).is_some());

        // Change the max age of the elements, all should be timed out
        sut.max_duration = Duration::from_millis(100);
        assert_eq!(sut.insert(0, 0), Ok(None));
        assert_eq!(sut.insert(1, 0), Ok(None));
        advance(Duration::from_millis(200)).await;

        // A full map of expired items takes a new one.
        assert_eq!(sut.insert(2, 0), Ok(None));
        assert_eq!(sut.len(), 1);
        assert!(sut.remove(&0).is_none());

        // The last element should be also timed out if we wait
        advance(Duration::from_millis(200)).await;
        assert!(sut.remove(&2).is_none());

        // Removing any key removes all expired elements.
        assert_eq!(sut.insert(0, 0), Ok(None));
        assert_eq!(sut.insert(1, 0), Ok(None));
        advance(Duration::from_millis(200)).await;
        assert!(sut.remove(&2).is_none());
        assert_eq!(sut.len(), 0);

        // So does a sweep.
        assert_eq!(sut.insert(0, 0), Ok(None));
        advance(Duration::from_millis(200)).await;
        assert!(!sut.is_empty());
        sut.sweep();
        assert!(sut.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn expired_elements_are_not_handed_out() {
        let mut sut = BoundedHashMap::default();
        assert_eq!(sut.insert(1, "one"), Ok(None));
        assert_eq!(sut.insert(2, "two"), Ok(None));
        advance(sut.max_duration + Duration::from_secs(1)).await;
        assert_eq!(sut.insert(3, "three"), Ok(None));

        // The expired element is taken, but not handed out, the other expired
        // one goes as well.
        assert_eq!(sut.remove(&1), None);
        assert_eq!(sut.len(), 1);
        assert_eq!(sut.remove(&3), Some("three"));
    }
}
