//! A single inotify instance which watches the memory events files of all
//! containers.

use crate::{
    async_fd::{self, Fd},
    mutex::lock,
};
use anyhow::{Context, Result};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor};
use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{
        Arc, Mutex, PoisonError, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{
    io::{ErrorKind, Interest, unix::AsyncFd},
    runtime::Handle,
    sync::mpsc::Sender,
    task::{self, JoinHandle},
};
use tracing::{error, trace};

/// A watch event as delivered to subscribers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WatchEvent {
    /// The file which changed. None if the kernel dropped events, every
    /// watched file may have changed then.
    pub(super) path: Option<Arc<Path>>,

    /// Whether the file got removed, which ends its watch.
    pub(super) removed: bool,
}

impl WatchEvent {
    /// Events got lost, by the kernel or a full channel.
    pub(super) const LOST: Self = Self {
        path: None,
        removed: false,
    };
}

/// What a watch reports. Opening a file is no change, the watcher reads the
/// files itself.
const WATCH_FLAGS: AddWatchFlags = AddWatchFlags::IN_MODIFY
    .union(AddWatchFlags::IN_ATTRIB)
    .union(AddWatchFlags::IN_CLOSE_WRITE)
    .union(AddWatchFlags::IN_DELETE_SELF)
    .union(AddWatchFlags::IN_MOVE_SELF);

/// The watches by descriptor. A watch belongs to a file rather than to its
/// path: a file which got recreated gets a watch of its own, and the events
/// of the old one, like its removal, must not reach the subscribers of the
/// new one.
#[derive(Default)]
struct Watches(HashMap<WatchDescriptor, Watch>);

/// A watch of a file and its subscribers.
struct Watch {
    path: Arc<Path>,
    subscribers: Vec<(u64, Sender<WatchEvent>)>,
}

impl Watches {
    /// Hand an event of the kernel to the subscribers of its watch.
    fn dispatch(&mut self, event: &InotifyEvent) {
        if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
            self.lost();
            return;
        }
        let Some(watch) = self.0.get(&event.wd) else {
            trace!("Watch event with unknown descriptor: {event:?}");
            return;
        };
        // A removal of a cgroup only removes the watch, which tells nothing
        // about the file, see [`OOMWatcher::watch_cgroup_v2`].
        if event.mask.intersects(WATCH_FLAGS) {
            let watch_event = WatchEvent {
                path: Some(watch.path.clone()),
                removed: event.mask.contains(AddWatchFlags::IN_DELETE_SELF),
            };
            for (_, tx) in &watch.subscribers {
                Self::deliver(tx, &watch_event);
            }
        }
        // The last event of a watch, the kernel may reuse the descriptor.
        if event.mask.contains(AddWatchFlags::IN_IGNORED) {
            self.0.remove(&event.wd);
        }
    }

    /// Tell every subscriber that events got lost, which may concern every
    /// file.
    fn lost(&self) {
        for (_, tx) in self.0.values().flat_map(|watch| &watch.subscribers) {
            Self::deliver(tx, &WatchEvent::LOST);
        }
    }

    /// Deliver an event without ever blocking the reader of the events. The
    /// last free slot of a channel takes a lost event instead, so that a
    /// full channel drops events without its receiver missing that it did:
    /// a dropped event of the pod cgroup must still refresh its baseline.
    fn deliver(tx: &Sender<WatchEvent>, event: &WatchEvent) {
        let event = if tx.capacity() > 1 {
            event.clone()
        } else {
            WatchEvent::LOST
        };
        if tx.try_send(event).is_err() {
            trace!("Dropping watch event, subscriber not ready");
        }
    }
}

/// A single inotify instance shared by all OOM watchers.
///
/// Creating an inotify instance per container does not scale with the number
/// of containers in a pod. All memory event files are watched through this
/// instance instead, a task reads its events and dispatches them to the
/// subscribers of their path. Adding and removing a watch does not block, the
/// subscribers do that right away.
pub(super) struct SharedWatcher {
    inotify: Arc<AsyncFd<Fd<Inotify>>>,

    /// The subscribers and the watches. The reader of the events takes this
    /// lock as well, it never waits while holding it.
    watches: Arc<Mutex<Watches>>,

    /// Set once the reader of the events stopped, the next subscription gets
    /// a new watcher then.
    failed: Arc<AtomicBool>,

    /// The task which reads the events, it ends with the watcher.
    reader: JoinHandle<()>,

    /// Source of unique subscription identifiers.
    next_id: AtomicU64,
}

/// Where the shared watcher is kept, see [`SharedWatcher::get`].
type Slot = Mutex<Weak<SharedWatcher>>;

/// The shared watcher is kept alive by its subscriptions. It gets created on
/// the first subscription and dropped with the last one, which frees the
/// inotify instance while no container is watched. A failed creation is
/// retried on the next subscription instead of being cached, and so is a
/// watcher whose reader stopped. The server has a single runtime, which reads
/// the events.
#[cfg(not(test))]
static SHARED_WATCHER: Slot = Mutex::new(Weak::new());

/// The unit tests run a runtime per test, each one gets a watcher of its own,
/// which its runtime reads.
#[cfg(test)]
static SHARED_WATCHERS: Mutex<Vec<(tokio::runtime::Id, &'static Slot)>> = Mutex::new(Vec::new());

impl SharedWatcher {
    /// Retrieve the shared watcher, creating it if necessary.
    pub(super) fn get() -> Result<Arc<Self>> {
        // The reader of the events is a task of the runtime.
        let runtime = Handle::try_current().context("get runtime")?;
        Self::get_in(Self::slot(&runtime)?)
    }

    /// Retrieve the watcher kept in the slot, or create one there.
    fn get_in(slot: &Slot) -> Result<Arc<Self>> {
        let mut slot = lock(slot)?;
        if let Some(watcher) = slot
            .upgrade()
            .filter(|watcher| !watcher.failed.load(Ordering::Acquire))
        {
            return Ok(watcher);
        }
        let watcher = Arc::new(Self::new().context("create shared file watcher")?);
        *slot = Arc::downgrade(&watcher);
        Ok(watcher)
    }

    #[cfg(not(test))]
    fn slot(_: &Handle) -> Result<&'static Slot> {
        Ok(&SHARED_WATCHER)
    }

    #[cfg(test)]
    fn slot(runtime: &Handle) -> Result<&'static Slot> {
        let id = runtime.id();
        let mut shared = lock(&SHARED_WATCHERS)?;
        if let Some((_, slot)) = shared.iter().find(|(i, _)| *i == id) {
            return Ok(slot);
        }
        // One per test, which lives as long as the static.
        let slot = Box::leak(Box::default());
        shared.push((id, slot));
        Ok(slot)
    }

    fn new() -> Result<Self> {
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)
            .context("create inotify instance")?;
        let inotify =
            async_fd::register(inotify, Interest::READABLE).context("register inotify instance")?;
        let inotify = Arc::new(inotify);
        let watches = Arc::<Mutex<Watches>>::default();
        let failed = Arc::<AtomicBool>::default();
        let reader = task::spawn(Self::read_events(
            inotify.clone(),
            watches.clone(),
            failed.clone(),
        ));
        Ok(Self {
            inotify,
            watches,
            failed,
            reader,
            next_id: AtomicU64::new(0),
        })
    }

    /// Read the events of the kernel and dispatch them, until the watcher
    /// got dropped or reading failed.
    async fn read_events(
        inotify: Arc<AsyncFd<Fd<Inotify>>>,
        watches: Arc<Mutex<Watches>>,
        failed: Arc<AtomicBool>,
    ) {
        loop {
            let events = match inotify
                .async_io(Interest::READABLE, |fd| {
                    fd.read_events().map_err(io::Error::from)
                })
                .await
            {
                Ok(events) => events,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    error!("Unable to read watch events, checking on exit only: {e}");
                    break;
                }
            };
            let Ok(mut watches) = lock(&watches) else {
                break;
            };
            for event in &events {
                watches.dispatch(event);
            }
        }
        Self::stopped(&watches, &failed);
    }

    /// The reader stopped. The next subscription gets a new watcher. The
    /// watches get dropped with their senders, so that the channels of the
    /// subscribers end once they took the events so far, they watch again
    /// then.
    fn stopped(watches: &Mutex<Watches>, failed: &AtomicBool) {
        failed.store(true, Ordering::Release);
        let watches = std::mem::take(&mut *watches.lock().unwrap_or_else(PoisonError::into_inner));
        drop(watches);
    }

    /// Start watching `path` on behalf of `tx`. The watch gets removed when the
    /// returned subscription is dropped and no other subscriber of the watch
    /// is left.
    pub(super) fn subscribe(
        self: &Arc<Self>,
        path: &Path,
        tx: Sender<WatchEvent>,
    ) -> Result<Subscription> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut watches = lock(&self.watches)?;

        // inotify returns the existing descriptor for a file which is watched
        // already, the subscriber joins its watch then.
        let wd = self
            .inotify
            .get_ref()
            .add_watch(path, WATCH_FLAGS)
            .with_context(|| format!("watch {}", path.display()))?;
        watches
            .0
            .entry(wd)
            .or_insert_with(|| Watch {
                path: path.into(),
                subscribers: vec![],
            })
            .subscribers
            .push((id, tx));

        Ok(Subscription {
            watcher: self.clone(),
            id,
            wd,
        })
    }

    fn unsubscribe(&self, id: u64, wd: WatchDescriptor) {
        let Ok(mut watches) = self.watches.lock() else {
            return;
        };
        // The kernel removed the watch already.
        let Some(watch) = watches.0.get_mut(&wd) else {
            return;
        };
        watch.subscribers.retain(|(i, _)| *i != id);
        if !watch.subscribers.is_empty() {
            return;
        }
        let Some(watch) = watches.0.remove(&wd) else {
            return;
        };
        if let Err(e) = self.inotify.get_ref().rm_watch(wd) {
            // The watch is gone already if the cgroup got removed.
            trace!("Unable to unwatch {}: {:#}", watch.path.display(), e);
        }
    }
}

impl Drop for SharedWatcher {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// A subscription to the [`SharedWatcher`] which unsubscribes on drop.
pub(super) struct Subscription {
    watcher: Arc<SharedWatcher>,
    id: u64,
    wd: WatchDescriptor,
}

impl Subscription {
    /// Whether the reader of the watcher stopped, which ended the watch.
    pub(super) fn watcher_failed(&self) -> bool {
        self.watcher.failed.load(Ordering::Acquire)
    }
}

/// Ways for the tests of the watches to act like the kernel.
#[cfg(test)]
impl SharedWatcher {
    /// Stop the reader, like a failure to read the events does.
    pub(super) fn stop(&self) {
        Self::stopped(&self.watches, &self.failed);
    }

    /// Remove the watch of a file, which the kernel reports with IN_IGNORED
    /// only, like for the files of a cgroup which got removed.
    pub(super) fn remove_watch(&self, path: &Path) -> Result<()> {
        let wd = lock(&self.watches)?
            .0
            .iter()
            .find(|(_, watch)| *watch.path == *path)
            .map(|(wd, _)| *wd)
            .context("not watched")?;
        Ok(self.inotify.get_ref().rm_watch(wd)?)
    }

    /// Whether the file is watched.
    pub(super) fn watches(&self, path: &Path) -> bool {
        lock(&self.watches)
            .is_ok_and(|watches| watches.0.values().any(|watch| *watch.path == *path))
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.watcher.unsubscribe(self.id, self.wd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oom_watcher::WATCH_EVENT_CHANNEL_SIZE;
    use nix::sys::inotify::InitFlags;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use tokio::{
        sync::mpsc::{Receiver, channel},
        time::{Duration, sleep, timeout},
    };

    async fn wait_for_event(rx: &mut Receiver<WatchEvent>) -> Result<WatchEvent> {
        timeout(Duration::from_secs(5), rx.recv())
            .await
            .context("no event within timeout")?
            .context("channel closed")
    }

    /// The number of subscribers of a path.
    fn subscribers(watcher: &SharedWatcher, path: &Path) -> Result<usize> {
        Ok(lock(&watcher.watches)?
            .0
            .values()
            .filter(|watch| *watch.path == *path)
            .map(|watch| watch.subscribers.len())
            .sum())
    }

    #[test]
    fn full_channel_ends_with_a_lost_event() {
        let (tx, mut rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let event = WatchEvent {
            path: Some(Path::new("/memory.events").into()),
            removed: false,
        };
        for _ in 0..2 * WATCH_EVENT_CHANNEL_SIZE {
            Watches::deliver(&tx, &event);
        }
        let mut received = vec![];
        while let Ok(event) = rx.try_recv() {
            received.push(event);
        }
        assert_eq!(received.len(), WATCH_EVENT_CHANNEL_SIZE);
        assert_eq!(received.pop(), Some(WatchEvent::LOST));
        assert!(received.iter().all(|received| *received == event));

        // Once there is room again, events get delivered as they are.
        Watches::deliver(&tx, &event);
        assert_eq!(rx.try_recv().ok(), Some(event));
    }

    #[tokio::test]
    async fn shared_watcher_dispatches_by_path() -> Result<()> {
        let mut first = NamedTempFile::new()?;
        let second = NamedTempFile::new()?;
        let watcher = SharedWatcher::get()?;

        let (tx1, mut rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let (tx2, mut rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let _sub1 = watcher.subscribe(first.path(), tx1)?;
        let _sub2 = watcher.subscribe(second.path(), tx2)?;

        first.write_all(b"oom 1\n")?;
        first.flush()?;

        let event = wait_for_event(&mut rx1).await?;
        assert_eq!(event.path.as_deref(), Some(first.path()));
        assert!(!event.removed);
        assert!(rx2.try_recv().is_err());

        Ok(())
    }

    #[tokio::test]
    async fn shared_watcher_reports_removals() -> Result<()> {
        let file = NamedTempFile::new()?;
        let watcher = SharedWatcher::get()?;
        let (tx, mut rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let _sub = watcher.subscribe(file.path(), tx)?;

        let path = file.path().to_path_buf();
        file.close()?;
        // The link count changes first.
        loop {
            let event = wait_for_event(&mut rx).await?;
            assert_eq!(event.path.as_deref(), Some(path.as_path()));
            if event.removed {
                break;
            }
        }
        // The kernel dropped the watch, the descriptor is forgotten.
        timeout(Duration::from_secs(5), async {
            while lock(&watcher.watches).is_ok_and(|watches| !watches.0.is_empty()) {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .context("watch not forgotten")?;
        Ok(())
    }

    #[test]
    fn watches_keep_events_of_recreated_files_apart() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("memory.events");
        let inotify = Inotify::init(InitFlags::IN_CLOEXEC)?;
        std::fs::write(&path, "")?;
        let old = inotify.add_watch(&path, WATCH_FLAGS)?;
        std::fs::remove_file(&path)?;
        std::fs::write(&path, "")?;
        let new = inotify.add_watch(&path, WATCH_FLAGS)?;
        assert_ne!(old, new);

        let mut watches = Watches::default();
        let (old_tx, mut old_rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let (new_tx, mut new_rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        for (wd, tx) in [(old, old_tx), (new, new_tx)] {
            watches.0.insert(
                wd,
                Watch {
                    path: path.as_path().into(),
                    subscribers: vec![(0, tx)],
                },
            );
        }

        // The removal of the old file comes after the new one got watched.
        for mask in [AddWatchFlags::IN_DELETE_SELF, AddWatchFlags::IN_IGNORED] {
            watches.dispatch(&InotifyEvent {
                wd: old,
                mask,
                cookie: 0,
                name: None,
            });
        }
        assert!(old_rx.try_recv()?.removed);
        assert!(new_rx.try_recv().is_err());
        assert!(!watches.0.contains_key(&old));
        assert!(watches.0.contains_key(&new));
        Ok(())
    }

    #[test]
    fn lost_events_reach_every_subscriber() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let inotify = Inotify::init(InitFlags::IN_CLOEXEC)?;
        let mut watches = Watches::default();
        let mut receivers = vec![];
        for name in ["a", "b"] {
            let path = dir.path().join(name);
            std::fs::write(&path, "")?;
            let wd = inotify.add_watch(&path, WATCH_FLAGS)?;
            let (tx, rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
            watches.0.insert(
                wd,
                Watch {
                    path: path.as_path().into(),
                    subscribers: vec![(0, tx)],
                },
            );
            receivers.push((wd, rx));
        }

        // The kernel reports an overflow of its queue without a watch.
        watches.dispatch(&InotifyEvent {
            wd: receivers[0].0,
            mask: AddWatchFlags::IN_Q_OVERFLOW,
            cookie: 0,
            name: None,
        });
        for (_, rx) in &mut receivers {
            let event = rx.try_recv()?;
            assert_eq!(event.path, None);
            assert!(!event.removed);
        }
        assert_eq!(watches.0.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn shared_watcher_ends_watches_on_ignored() -> Result<()> {
        let file = NamedTempFile::new()?;
        let watcher = SharedWatcher::get()?;
        let (tx, mut rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let sub = watcher.subscribe(file.path(), tx)?;

        // cgroupfs only sends IN_IGNORED for the files of a removed cgroup,
        // like removing the watch does.
        watcher.inotify.get_ref().rm_watch(sub.wd)?;
        assert_eq!(timeout(Duration::from_secs(5), rx.recv()).await?, None);
        assert!(lock(&watcher.watches)?.0.is_empty());
        drop(sub);
        Ok(())
    }

    #[tokio::test]
    async fn stopped_watchers_get_replaced() -> Result<()> {
        let file = NamedTempFile::new()?;
        let watcher = SharedWatcher::get()?;
        let (tx, mut rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let _sub = watcher.subscribe(file.path(), tx)?;

        SharedWatcher::stopped(&watcher.watches, &watcher.failed);

        // The watches got dropped with their senders, which ends the channels
        // of the subscribers.
        assert!(lock(&watcher.watches)?.0.is_empty());
        assert_eq!(timeout(Duration::from_secs(5), rx.recv()).await?, None);
        // The next subscription gets a watcher which reads its events.
        assert!(!Arc::ptr_eq(&SharedWatcher::get()?, &watcher));
        Ok(())
    }

    /// The way the server keeps its single watcher, which the other tests
    /// take from a slot per runtime.
    #[tokio::test]
    async fn shared_watcher_slot() -> Result<()> {
        let slot = Slot::default();
        let first = SharedWatcher::get_in(&slot)?;
        assert!(Arc::ptr_eq(&first, &SharedWatcher::get_in(&slot)?));

        // One whose reader stopped gets replaced.
        SharedWatcher::stopped(&first.watches, &first.failed);
        let second = SharedWatcher::get_in(&slot)?;
        assert!(!Arc::ptr_eq(&first, &second));

        // It goes with its last user, the next one reads events again.
        let gone = Arc::downgrade(&second);
        drop((first, second));
        assert_eq!(gone.strong_count(), 0);
        let mut file = NamedTempFile::new()?;
        let (tx, mut rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let _sub = SharedWatcher::get_in(&slot)?.subscribe(file.path(), tx)?;
        file.write_all(b"oom 1\n")?;
        file.flush()?;
        wait_for_event(&mut rx).await?;
        Ok(())
    }

    #[tokio::test]
    async fn shared_watcher_unsubscribes_on_drop() -> Result<()> {
        let file = NamedTempFile::new()?;
        let watcher = SharedWatcher::get()?;

        let (tx1, _rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let (tx2, _rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let sub1 = watcher.subscribe(file.path(), tx1)?;
        let sub2 = watcher.subscribe(file.path(), tx2)?;
        assert_eq!(subscribers(&watcher, file.path())?, 2);

        drop(sub1);
        assert_eq!(subscribers(&watcher, file.path())?, 1);

        drop(sub2);
        assert!(lock(&watcher.watches)?.0.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn shared_watchers_go_with_their_last_subscription() -> Result<()> {
        let file = NamedTempFile::new()?;
        let (tx, _rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let sub = SharedWatcher::get()?.subscribe(file.path(), tx)?;
        let watcher = Arc::downgrade(&sub.watcher);
        assert!(Arc::ptr_eq(&SharedWatcher::get()?, &sub.watcher));
        drop(sub);
        assert_eq!(watcher.strong_count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn shared_watcher_resubscribe_same_path() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        let watcher = SharedWatcher::get()?;

        let (tx1, _rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let _sub1 = watcher.subscribe(file.path(), tx1)?;

        // A second subscription re-adds the watch and still gets events.
        let (tx2, mut rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let _sub2 = watcher.subscribe(file.path(), tx2)?;

        file.write_all(b"oom 1\n")?;
        file.flush()?;

        wait_for_event(&mut rx2).await?;

        Ok(())
    }

    #[tokio::test]
    async fn shared_watcher_keeps_watch_after_other_subscriber_left() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        let watcher = SharedWatcher::get()?;

        let (tx1, mut rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let _sub1 = watcher.subscribe(file.path(), tx1)?;

        // A second subscriber comes and goes, like an exec in the container.
        let (tx2, _rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let sub2 = watcher.subscribe(file.path(), tx2)?;
        drop(sub2);
        assert_eq!(subscribers(&watcher, file.path())?, 1);

        file.write_all(b"oom 1\n")?;
        file.flush()?;

        wait_for_event(&mut rx1).await?;

        Ok(())
    }
}
