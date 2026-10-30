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
        Arc, Mutex, Weak,
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
}

impl WatchEvent {
    /// Events got lost, by the kernel or a full channel.
    pub(super) const LOST: Self = Self { path: None };
}

/// What a watch reports. cgroupfs notifies a change of the memory events
/// with `IN_MODIFY` and the removal of a cgroup with `IN_IGNORED` only,
/// which every watch gets anyway.
const WATCH_FLAGS: AddWatchFlags = AddWatchFlags::IN_MODIFY;

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
        let mut slot = lock(slot);
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
        let mut shared = lock(&SHARED_WATCHERS);
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
            let mut watches = lock(&watches);
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
        let watches = std::mem::take(&mut *lock(watches));
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
        let mut watches = lock(&self.watches);

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
        let mut watches = lock(&self.watches);
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
        let wd = lock(&self.watches)
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
            .0
            .values()
            .any(|watch| *watch.path == *path)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.watcher.unsubscribe(self.id, self.wd);
    }
}

#[cfg(test)]
mod tests;
