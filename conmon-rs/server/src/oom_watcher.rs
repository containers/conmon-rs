use crate::mutex::lock;
use anyhow::{Context, Result, bail};
use linereader::LineReader;
use nix::sys::statfs::{FsType, statfs};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode},
};
use std::{
    collections::HashMap,
    fs::File as StdFile,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, ErrorKind},
    runtime::Handle,
    sync::mpsc::{Receiver, Sender, channel},
    task,
};
use tokio_eventfd::EventFd;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, trace};

pub const CGROUP2_SUPER_MAGIC: FsType = FsType(libc::CGROUP2_SUPER_MAGIC as _);

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

const MAX_LINEREADER_CAPACITY: usize = 256;

/// Capacity of the channel delivering file watch events to one OOM watcher.
const WATCH_EVENT_CHANNEL_SIZE: usize = 32;

static IS_CGROUP_V2: LazyLock<bool> = LazyLock::new(|| {
    statfs(CGROUP_ROOT).is_ok_and(|sts| sts.filesystem_type() == CGROUP2_SUPER_MAGIC)
});

pub struct OOMWatcher {
    token: CancellationToken,
}

/// The OOM related counters of a memory events file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OomCounters {
    /// Times the memory limit of the cgroup was hit.
    oom: u64,

    /// Processes killed because of it. Kills are accounted to the victim's
    /// leaf cgroup, so this drifts apart from `oom` by design and the two are
    /// compared separately.
    oom_kill: u64,
}

impl OomCounters {
    /// Whether any counter increased compared to `baseline`.
    fn increased_since(&self, baseline: &Self) -> bool {
        self.oom > baseline.oom || self.oom_kill > baseline.oom_kill
    }
}

#[derive(Debug)]
pub struct OOMEvent {
    pub oom: bool,
}

impl OOMWatcher {
    pub fn new(
        token: &CancellationToken,
        pid: u32,
        exit_paths: &[PathBuf],
        tx: Sender<OOMEvent>,
    ) -> OOMWatcher {
        let exit_paths = exit_paths.to_owned();
        let token = token.clone();
        let stop = token.clone();
        task::spawn(
            async move {
                if let Err(e) = if *IS_CGROUP_V2 {
                    Self::oom_handling_cgroup_v2(stop, pid, &exit_paths, tx)
                        .await
                        .context("setup cgroupv2 oom handling")
                } else {
                    Self::oom_handling_cgroup_v1(stop, pid, &exit_paths, tx)
                        .await
                        .context("setup cgroupv1 oom handling")
                } {
                    error!("Failed to watch OOM: {:#}", e);
                }
            }
            .instrument(debug_span!("cgroup_handling", pid)),
        );
        OOMWatcher { token }
    }

    pub fn stop(self) {
        self.token.cancel();
    }

    async fn oom_handling_cgroup_v1(
        token: CancellationToken,
        pid: u32,
        exit_paths: &[PathBuf],
        tx: Sender<OOMEvent>,
    ) -> Result<()> {
        let memory_cgroup_path = if let Some(path) =
            Self::process_cgroup_subsystem_path_cgroup_v1(pid, "memory")
                .context("process cgroup memory subsystem path")?
        {
            path
        } else {
            debug!("Stopping OOM handler because no cgroup subsystem path exists");
            return Ok(());
        };

        debug!(
            "Using memory cgroup v1 path: {}",
            memory_cgroup_path.display()
        );

        let memory_cgroup_file_oom_path = memory_cgroup_path.join("memory.oom_control");
        let event_control_path = memory_cgroup_path.join("cgroup.event_control");
        let path = memory_cgroup_file_oom_path.to_str();

        debug!(path, "Setup cgroup v1 oom handling");

        let oom_cgroup_file = OpenOptions::new()
            .write(true)
            .open(memory_cgroup_file_oom_path)
            .await
            .context("opening cgroup oom file")?;
        let mut oom_event_fd = EventFd::new(0, false).context("creating eventfd")?;

        let mut event_control = OpenOptions::new()
            .write(true)
            .open(event_control_path)
            .await
            .context("opening cgroup event control file")?;
        event_control
            .write_all(
                format!(
                    "{} {}",
                    oom_event_fd.as_raw_fd(),
                    oom_cgroup_file.as_raw_fd()
                )
                .as_bytes(),
            )
            .await
            .context("writing control data")?;
        event_control.flush().await.context("flush control data")?;

        debug!("Successfully setup cgroup v1 oom detection");

        let mut buffer = [0u8; 16];
        tokio::select! {
            _ = token.cancelled() => {
                debug!("Loop cancelled");
                let _ = tx.try_send(OOMEvent{ oom: false });
            }
            _ = oom_event_fd.read(&mut buffer) => {
                debug!("Got oom event");
                Self::write_oom_files(exit_paths).await;
                let _ = tx.try_send(OOMEvent{ oom: true });
            }
        }

        debug!("Done watching for ooms");
        Ok(())
    }

    async fn oom_handling_cgroup_v2(
        token: CancellationToken,
        pid: u32,
        exit_paths: &[PathBuf],
        tx: Sender<OOMEvent>,
    ) -> Result<()> {
        let subsystem_path = if let Some(path) = Self::process_cgroup_subsystem_path_cgroup_v2(pid)
            .context("process cgroup subsystem path")?
        {
            path
        } else {
            debug!("Stopping OOM handler because no cgroup subsystem path exists");
            return Ok(());
        };

        debug!(
            "Using subsystem cgroup v2 path: {}",
            subsystem_path.display()
        );

        let mut events = MemoryEvents::new(subsystem_path.join("memory.events"));

        // crun places the process in a leaf below the container cgroup, and
        // the OOMs end up recorded in that parent. Only follow the parent in
        // that layout, where it merely wraps our leaf. A pod cgroup shared with
        // sibling containers (runc) must not be watched, otherwise an OOM of a
        // sibling would be reported for this container. Prefer
        // memory.events.local, the counters in memory.events include events
        // from below, and fall back on kernels without the local file.
        let parent_path = subsystem_path
            .parent()
            .filter(|_| Self::is_only_child(&subsystem_path))
            .and_then(|parent| {
                [
                    parent.join("memory.events.local"),
                    parent.join("memory.events"),
                ]
                .into_iter()
                .find(|path| path.exists())
            });
        let mut parent_events = parent_path.map(MemoryEvents::new);

        let path = events.path.to_str();
        debug!(path, "Setup cgroup v2 handling");

        // Adding the watches runs on the blocking pool. The process may exit
        // in the meantime, which must be reported without waiting for a busy
        // pool.
        let mut subscriptions = tokio::select! {
            _ = token.cancelled() => {
                debug!("Cancelled while adding the watches");
                Self::last_resort_check(&subsystem_path, &mut events, parent_events.as_mut(), exit_paths, &tx).await;
                return Ok(());
            }
            res = Self::subscribe(&events, parent_events.as_ref()) => match res {
                Ok(subscriptions) => subscriptions,
                // The cgroup may be gone already. Still check for an OOM once
                // the process exited.
                Err(e) => {
                    debug!("Unable to watch memory events, checking on exit only: {:#}", e);
                    token.cancelled().await;
                    Self::last_resort_check(&subsystem_path, &mut events, parent_events.as_mut(), exit_paths, &tx).await;
                    return Ok(());
                }
            },
        };

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    debug!("Loop cancelled");
                    Self::last_resort_check(&subsystem_path, &mut events, parent_events.as_mut(), exit_paths, &tx).await;
                    break;
                }
                Some(res) = subscriptions.rx.recv() => {
                    let event = match res {
                        Ok(event) => event,
                        // Watcher errors without a path reach every subscriber of the
                        // shared watcher, so they must not end the detection for this
                        // container. The counters get re-read on the next event and on
                        // exit anyway.
                        Err(e) => {
                            debug!("Watch error: {:#}", e);
                            continue;
                        }
                    };

                    // Skip access events since they're not of any interest
                    if event.kind == EventKind::Access(AccessKind::Open(AccessMode::Any)) {
                        continue;
                    }
                    debug!("Got OOM file event: {:?}", event);
                    if event.kind.is_remove() {
                        debug!("Got remove event");
                        if let Err(e) = tx.try_send(OOMEvent{ oom: false }) {
                            error!("try_send failed: {:#}", e);
                        };
                        break
                    }

                    if Self::check_cgroups(&subsystem_path, &mut events, parent_events.as_mut()).await {
                        debug!("Writing OOM files");
                        Self::write_oom_files(exit_paths).await;
                        match tx.try_send(OOMEvent{ oom: true }) {
                            Ok(_) => break,
                            Err(e) => error!("try_send failed: {:#}", e)
                        };
                    } else if Self::file_not_found(&events.path).await {
                        debug!("Assuming memory slice removal race, still reporting one OOM event");
                        Self::write_oom_files(exit_paths).await;
                        match tx.try_send(OOMEvent{ oom: true }) {
                            Ok(_) => break,
                            Err(e) => error!("try_send failed: {:#}", e)
                        };
                    } else {
                        debug!("No OOM found in event");
                    };
                }
            }
        }

        debug!("Done watching for ooms");
        Ok(())
    }

    /// Watch the memory events of the container and, if present, the ones of
    /// its wrapper cgroup.
    async fn subscribe(
        events: &MemoryEvents,
        parent_events: Option<&MemoryEvents>,
    ) -> Result<Subscriptions> {
        let watcher = SharedWatcher::get()?;
        let (events_tx, rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let subscription = watcher
            .subscribe(&events.path, events_tx.clone())
            .await
            .context("watch memory events file")?;

        let mut parent_subscription = None;
        if let Some(parent) = parent_events {
            debug!(
                "Also watching parent memory events: {}",
                parent.path.display()
            );
            match watcher.subscribe(&parent.path, events_tx).await {
                Ok(subscription) => parent_subscription = Some(subscription),
                Err(e) => debug!("Could not watch parent memory events: {:#}", e),
            }
        }

        Ok(Subscriptions {
            rx,
            _subscription: subscription,
            _parent_subscription: parent_subscription,
        })
    }

    /// Check for an OOM a last time once the process exited, and report the
    /// result.
    async fn last_resort_check(
        leaf: &Path,
        events: &mut MemoryEvents,
        parent_events: Option<&mut MemoryEvents>,
        exit_paths: &[PathBuf],
        tx: &Sender<OOMEvent>,
    ) {
        debug!("Last resort check for OOM");
        let found_oom = Self::check_cgroups(leaf, events, parent_events).await;
        if found_oom {
            Self::write_oom_files(exit_paths).await;
        }
        if let Err(e) = tx.try_send(OOMEvent { oom: found_oom }) {
            error!("try_send failed: {:#}", e);
        }
    }

    /// Check the memory events of the container and of its wrapper cgroup for
    /// new OOMs. An OOM of the parent only counts while it wraps the container
    /// alone, otherwise it may stem from a sibling.
    async fn check_cgroups(
        leaf: &Path,
        events: &mut MemoryEvents,
        parent_events: Option<&mut MemoryEvents>,
    ) -> bool {
        if events.check() {
            return true;
        }
        let Some(parent) = parent_events else {
            return false;
        };
        // Only list the parent once its counters went up, memory events change
        // often under memory pressure. The counters are the baseline for the
        // next check either way, so an OOM skipped here is not reported later.
        if !parent.check() {
            return false;
        }
        if Self::is_only_child(leaf) {
            return true;
        }
        debug!("Ignoring OOM of parent cgroup with siblings");
        false
    }

    /// Read the current OOM counters of a memory events file to establish a
    /// baseline, so that only OOMs during this container's lifetime count.
    fn initial_oom_counters(path: &Path) -> OomCounters {
        match Self::read_oom_counters(path) {
            Ok(counters) => {
                debug!("Initial OOM counters of {}: {counters:?}", path.display());
                counters
            }
            Err(e) => {
                debug!(
                    "Could not read initial OOM counters of {}: {:#}, starting from 0",
                    path.display(),
                    e
                );
                OomCounters::default()
            }
        }
    }

    /// Whether the parent of `leaf` contains no other cgroup than `leaf`, so
    /// that it merely wraps it. The memory limit cannot tell the layouts
    /// apart, crun applies it to both the wrapper and the leaf. A parent
    /// without any child still counts, the leaf may already be gone when
    /// checking for an OOM after the exit.
    fn is_only_child(leaf: &Path) -> bool {
        let (Some(parent), Some(name)) = (leaf.parent(), leaf.file_name()) else {
            return false;
        };
        match std::fs::read_dir(parent) {
            Ok(entries) => entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|typ| typ.is_dir()))
                .all(|entry| entry.file_name() == name),
            Err(_) => false,
        }
    }

    /// Checks if a file does not exist on disk.
    async fn file_not_found(f: impl AsRef<Path>) -> bool {
        tokio::fs::metadata(f)
            .await
            .is_err_and(|e| e.kind() == ErrorKind::NotFound)
    }

    fn check_for_oom(
        memory_events_file_path: &Path,
        last: OomCounters,
    ) -> Result<(OomCounters, bool)> {
        debug!(
            "Checking for possible OOM in {}",
            memory_events_file_path.display()
        );
        let counters = Self::read_oom_counters(memory_events_file_path)?;
        let found_oom = counters.increased_since(&last);
        debug!("OOM counters {counters:?}, last {last:?}, found OOM: {found_oom}");
        Ok((counters, found_oom))
    }

    /// Parse the `oom` and `oom_kill` counters of a memory events file.
    fn read_oom_counters(path: &Path) -> Result<OomCounters> {
        let fp = StdFile::open(path)
            .with_context(|| format!("open memory events file: {}", path.display()))?;
        let mut reader = LineReader::with_capacity(MAX_LINEREADER_CAPACITY, fp);
        let mut counters = OomCounters::default();

        while let Some(l) = reader.next_line() {
            let line = str::from_utf8(l.context("read line from buffer")?)
                .context("convert line to utf8")?;
            trace!(line);

            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            let target = match key {
                "oom" => &mut counters.oom,
                "oom_kill" => &mut counters.oom_kill,
                _ => continue,
            };
            *target = value.trim_end().parse().context("parse u64 counter")?;
        }

        Ok(counters)
    }

    /// Create the OOM files. Failures are logged, the OOM gets reported
    /// through the channel either way.
    async fn write_oom_files(exit_paths: &[PathBuf]) {
        for path in exit_paths {
            let span = debug_span!("write_oom_file");
            async {
                debug!("Writing OOM file: {}", path.display());
                match File::create(path).await {
                    Ok(file) => {
                        // Ensure the file is synced to disk
                        if let Err(e) = file.sync_all().await {
                            error!("Could not sync oom file to {}: {:#}", path.display(), e);
                        } else {
                            debug!("Successfully wrote and synced OOM file: {}", path.display());
                        }
                    }
                    Err(e) => {
                        error!("Could not create oom file at {}: {:#}", path.display(), e);
                    }
                }
            }
            .instrument(span)
            .await;
        }
    }

    fn process_cgroup_subsystem_path_cgroup_v1(
        pid: u32,
        subsystem: &str,
    ) -> Result<Option<PathBuf>> {
        if let Some(fp) = Self::try_open_cgroup_path(pid)? {
            let mut reader = LineReader::with_capacity(MAX_LINEREADER_CAPACITY, fp);

            while let Some(line) = reader.next_line() {
                let mut iter = str::from_utf8(line.context("read line from buffer")?)
                    .context("convert line to utf8")?
                    .split(':')
                    .skip(1);

                let system = iter.next().context("no system found in cgroup")?;
                let path = iter
                    .next()
                    .context("no path found in cgroup")?
                    .strip_prefix("/")
                    .context("strip root path prefix")?
                    .trim_end();

                if system.contains(subsystem) || system.is_empty() {
                    return Ok(PathBuf::from(CGROUP_ROOT).join(subsystem).join(path).into());
                }
            }

            bail!("no path found")
        }

        Ok(None)
    }

    fn process_cgroup_subsystem_path_cgroup_v2(pid: u32) -> Result<Option<PathBuf>> {
        if let Some(fp) = Self::try_open_cgroup_path(pid)? {
            return Ok(Path::new(CGROUP_ROOT)
                .join(
                    str::from_utf8(
                        LineReader::with_capacity(MAX_LINEREADER_CAPACITY, fp)
                            .next_line()
                            .context("get next line")?
                            .context("read line from buffer")?,
                    )
                    .context("convert byte slice to utf8")?
                    .split(':')
                    .nth(2)
                    .context("no path found in cgroup")?
                    .strip_prefix("/")
                    .context("strip root path prefix")?
                    .trim_end(),
                )
                .into());
        }

        Ok(None)
    }

    fn try_open_cgroup_path(pid: u32) -> Result<Option<StdFile>> {
        let cgroup_path = PathBuf::from("/proc").join(pid.to_string()).join("cgroup");
        debug!("Using cgroup path: {}", cgroup_path.display());

        match StdFile::open(&cgroup_path) {
            Ok(file) => Ok(file.into()),
            // Short lived processes will not be handled as an error
            Err(error) if error.kind() == ErrorKind::NotFound => {
                trace!("Cgroup path not found: {}", cgroup_path.display());
                Ok(None)
            }
            Err(error) => bail!("open cgroup path {}: {}", cgroup_path.display(), error),
        }
    }
}

/// A memory events file and the OOM counters seen in it so far.
struct MemoryEvents {
    path: PathBuf,
    last: OomCounters,
}

impl MemoryEvents {
    /// Track `path`, starting from its current counters.
    fn new(path: PathBuf) -> Self {
        let last = OOMWatcher::initial_oom_counters(&path);
        Self { path, last }
    }

    /// Whether the counters increased since the last OOM found.
    fn check(&mut self) -> bool {
        match OOMWatcher::check_for_oom(&self.path, self.last) {
            Ok((counters, true)) => {
                debug!("Found OOM in {}, count {counters:?}", self.path.display());
                self.last = counters;
                true
            }
            Ok(_) => false,
            Err(e) => {
                debug!("Checking {} for OOM failed: {:#}", self.path.display(), e);
                false
            }
        }
    }
}

/// The watches of a single OOM watcher and the receiver of their events. The
/// watches get removed on drop.
struct Subscriptions {
    rx: Receiver<WatchResult>,
    _subscription: Subscription,
    _parent_subscription: Option<Subscription>,
}

/// A watch event as delivered to subscribers. Errors are shared because
/// `notify::Error` is not `Clone`.
type WatchResult = Result<Event, Arc<notify::Error>>;

type Subscribers = Mutex<HashMap<PathBuf, Vec<(u64, Sender<WatchResult>)>>>;

/// A single inotify instance shared by all OOM watchers.
///
/// Every `notify` watcher owns an inotify file descriptor and a thread, so
/// creating one per container does not scale with the number of containers in
/// a pod. All memory event files are watched through this instance instead and
/// the events get dispatched to the subscribers of their path.
struct SharedWatcher {
    /// The underlying watcher. Watches get added and removed while holding
    /// this lock, which also serializes the decision whether a path still has
    /// subscribers. The event callback never takes it.
    watcher: Mutex<RecommendedWatcher>,

    /// Subscribers by watched path. This is the only lock taken by the event
    /// callback, so it must never be held while calling into the watcher.
    subscribers: Arc<Subscribers>,

    /// Source of unique subscription identifiers.
    next_id: AtomicU64,
}

/// The shared watcher is kept alive by its subscriptions. It gets created on
/// the first subscription and dropped with the last one, which frees the
/// inotify instance and its thread while no container is watched. A failed
/// creation is retried on the next subscription instead of being cached.
static SHARED_WATCHER: Mutex<Weak<SharedWatcher>> = Mutex::new(Weak::new());

impl SharedWatcher {
    /// Retrieve the process wide shared watcher, creating it if necessary.
    fn get() -> Result<Arc<Self>> {
        let mut shared = lock(&SHARED_WATCHER)?;
        if let Some(watcher) = shared.upgrade() {
            return Ok(watcher);
        }
        let watcher = Arc::new(Self::new().context("create shared file watcher")?);
        *shared = Arc::downgrade(&watcher);
        Ok(watcher)
    }

    fn new() -> Result<Self> {
        let subscribers: Arc<Subscribers> = Arc::default();
        let callback_subscribers = subscribers.clone();

        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(subscribers) = callback_subscribers.lock() else {
                return;
            };

            let (paths, res) = match res {
                Ok(event) => (event.paths.clone(), Ok(event)),
                Err(e) => (e.paths.clone(), Err(Arc::new(e))),
            };

            if paths.is_empty() {
                // Errors without a path concern the watcher as a whole.
                for (_, tx) in subscribers.values().flatten() {
                    Self::deliver(tx, &res);
                }
                return;
            }

            for path in &paths {
                match subscribers.get(path) {
                    Some(subs) => subs.iter().for_each(|(_, tx)| Self::deliver(tx, &res)),
                    None => trace!("No subscriber for watch event on {}", path.display()),
                }
            }
        })
        .context("create file watcher")?;

        Ok(Self {
            watcher: Mutex::new(watcher),
            subscribers,
            next_id: AtomicU64::new(0),
        })
    }

    /// Deliver an event without ever blocking the watcher thread. A full
    /// channel drops the event, the receiver re-reads the counters on the
    /// next one anyway.
    fn deliver(tx: &Sender<WatchResult>, res: &WatchResult) {
        if tx.try_send(res.clone()).is_err() {
            trace!("Dropping watch event, subscriber not ready");
        }
    }

    /// Start watching `path` on behalf of `tx`. The watch gets removed when the
    /// returned subscription is dropped and no other subscriber for the path
    /// is left.
    ///
    /// Adding a watch waits for a reply from notify's inotify thread, so it
    /// runs on the blocking pool rather than on the async thread.
    async fn subscribe(
        self: &Arc<Self>,
        path: &Path,
        tx: Sender<WatchResult>,
    ) -> Result<Subscription> {
        let watcher = self.clone();
        let path = path.to_path_buf();
        task::spawn_blocking(move || watcher.subscribe_blocking(&path, tx))
            .await
            .context("join subscribe task")?
    }

    fn subscribe_blocking(
        self: &Arc<Self>,
        path: &Path,
        tx: Sender<WatchResult>,
    ) -> Result<Subscription> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut watcher = lock(&self.watcher)?;

        lock(&self.subscribers)?
            .entry(path.to_path_buf())
            .or_default()
            .push((id, tx));

        // Add the watch even if the path has other subscribers. inotify returns
        // the existing descriptor for a watched path, and this re-establishes a
        // watch the kernel dropped (for example on DELETE_SELF) while a stale
        // subscriber with a full channel never saw the remove event.
        if let Err(e) = watcher.watch(path, RecursiveMode::NonRecursive) {
            drop(watcher);
            self.unsubscribe(id, path);
            return Err(e).with_context(|| format!("watch {}", path.display()));
        }

        Ok(Subscription {
            watcher: self.clone(),
            id,
            path: path.to_path_buf(),
        })
    }

    fn unsubscribe(&self, id: u64, path: &Path) {
        let Ok(mut watcher) = self.watcher.lock() else {
            return;
        };

        let last = {
            let Ok(mut subscribers) = self.subscribers.lock() else {
                return;
            };
            let Some(subs) = subscribers.get_mut(path) else {
                return;
            };
            subs.retain(|(i, _)| *i != id);
            if subs.is_empty() {
                subscribers.remove(path);
                true
            } else {
                false
            }
        };

        if last && let Err(e) = watcher.unwatch(path) {
            // The watch is gone already if the cgroup got removed.
            trace!("Unable to unwatch {}: {:#}", path.display(), e);
        }
    }
}

/// A subscription to the [`SharedWatcher`] which unsubscribes on drop.
struct Subscription {
    watcher: Arc<SharedWatcher>,
    id: u64,
    path: PathBuf,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let watcher = self.watcher.clone();
        let id = self.id;
        let path = std::mem::take(&mut self.path);
        // Removing the watch waits for notify's inotify thread, keep that off
        // the async thread if there is a runtime to hand it to.
        match Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(move || watcher.unsubscribe(id, &path));
            }
            Err(_) => watcher.unsubscribe(id, &path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use tokio::time::{Duration, sleep, timeout};

    async fn wait_for_event(rx: &mut tokio::sync::mpsc::Receiver<WatchResult>) -> Result<Event> {
        timeout(Duration::from_secs(5), rx.recv())
            .await
            .context("no event within timeout")?
            .context("channel closed")?
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    #[tokio::test]
    async fn shared_watcher_dispatches_by_path() -> Result<()> {
        let mut first = NamedTempFile::new()?;
        let second = NamedTempFile::new()?;
        let watcher = SharedWatcher::get()?;

        let (tx1, mut rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let (tx2, mut rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let _sub1 = watcher.subscribe(first.path(), tx1).await?;
        let _sub2 = watcher.subscribe(second.path(), tx2).await?;

        first.write_all(b"oom 1\n")?;
        first.flush()?;

        let event = wait_for_event(&mut rx1).await?;
        assert!(event.paths.iter().any(|p| p == first.path()));
        assert!(rx2.try_recv().is_err());

        Ok(())
    }

    #[tokio::test]
    async fn shared_watcher_unsubscribes_on_drop() -> Result<()> {
        let file = NamedTempFile::new()?;
        let watcher = SharedWatcher::get()?;

        let (tx1, _rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let (tx2, _rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let sub1 = watcher.subscribe(file.path(), tx1).await?;
        let sub2 = watcher.subscribe(file.path(), tx2).await?;
        assert_eq!(lock(&watcher.subscribers)?[file.path()].len(), 2);

        // Unsubscribing happens on the blocking pool, give it a moment.
        let subscribers = |path: &Path| -> Result<usize> {
            Ok(lock(&watcher.subscribers)?
                .get(path)
                .map_or(0, |subs| subs.len()))
        };
        drop(sub1);
        for _ in 0..200 {
            if subscribers(file.path())? == 1 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(subscribers(file.path())?, 1);

        drop(sub2);
        for _ in 0..200 {
            if subscribers(file.path())? == 0 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert!(!lock(&watcher.subscribers)?.contains_key(file.path()));

        Ok(())
    }

    #[tokio::test]
    async fn shared_watcher_resubscribe_same_path() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        let watcher = SharedWatcher::get()?;

        let (tx1, _rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let _sub1 = watcher.subscribe(file.path(), tx1).await?;

        // A second subscription re-adds the watch and still gets events.
        let (tx2, mut rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let _sub2 = watcher.subscribe(file.path(), tx2).await?;

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
        let _sub1 = watcher.subscribe(file.path(), tx1).await?;

        // A second subscriber comes and goes, like an exec in the container.
        let (tx2, _rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let sub2 = watcher.subscribe(file.path(), tx2).await?;
        drop(sub2);
        sleep(Duration::from_millis(200)).await;

        file.write_all(b"oom 1\n")?;
        file.flush()?;

        wait_for_event(&mut rx1).await?;

        Ok(())
    }

    #[test]
    fn only_child_of_wrapper_cgroup() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let leaf = parent.path().join("container");
        std::fs::create_dir(&leaf)?;
        std::fs::write(parent.path().join("memory.events"), "oom 0\n")?;
        assert!(OOMWatcher::is_only_child(&leaf));

        std::fs::create_dir(parent.path().join("sibling"))?;
        assert!(!OOMWatcher::is_only_child(&leaf));

        assert!(!OOMWatcher::is_only_child(
            &parent.path().join("missing").join("container")
        ));

        std::fs::remove_dir(&leaf)?;
        std::fs::remove_dir(parent.path().join("sibling"))?;
        assert!(OOMWatcher::is_only_child(&leaf));
        Ok(())
    }

    #[tokio::test]
    async fn parent_oom_ignored_with_siblings() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let leaf = parent.path().join("container");
        std::fs::create_dir(&leaf)?;
        std::fs::write(leaf.join("memory.events"), "oom 0\noom_kill 0\n")?;
        let parent_file = parent.path().join("memory.events.local");
        std::fs::write(&parent_file, "oom 0\noom_kill 0\n")?;

        let mut events = MemoryEvents::new(leaf.join("memory.events"));
        let mut parent_events = MemoryEvents::new(parent_file.clone());
        let mut check =
            async || OOMWatcher::check_cgroups(&leaf, &mut events, Some(&mut parent_events)).await;

        // An OOM recorded in the wrapper is found although the leaf is unchanged.
        std::fs::write(&parent_file, "oom 1\noom_kill 1\n")?;
        assert!(check().await);
        assert!(!check().await);

        // While a sibling exists, its OOMs are not attributed to this container.
        let sibling = parent.path().join("sibling");
        std::fs::create_dir(&sibling)?;
        std::fs::write(&parent_file, "oom 2\noom_kill 2\n")?;
        assert!(!check().await);

        // Once it is gone again, neither are its earlier OOMs, only new ones.
        std::fs::remove_dir(&sibling)?;
        assert!(!check().await);
        std::fs::write(&parent_file, "oom 3\noom_kill 3\n")?;
        assert!(check().await);
        Ok(())
    }

    #[test]
    fn read_oom_counters_separately() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        file.write_all(b"low 0\nhigh 0\nmax 3\noom 1\noom_kill 0\noom_group_kill 0\n")?;
        file.flush()?;

        let counters = OOMWatcher::read_oom_counters(file.path())?;
        assert_eq!(
            counters,
            OomCounters {
                oom: 1,
                oom_kill: 0
            }
        );

        // Unchanged counters are no OOM, even though they differ from each other.
        assert!(!counters.increased_since(&counters));
        assert!(
            OomCounters {
                oom: 1,
                oom_kill: 1
            }
            .increased_since(&counters)
        );
        assert!(
            !OomCounters {
                oom: 0,
                oom_kill: 0
            }
            .increased_since(&counters)
        );

        Ok(())
    }
}
