//! The memory events of a cgroup v2 and of the cgroups above it.

use super::{
    OOMEvent, OOMWatcher, WATCH_EVENT_CHANNEL_SIZE,
    inotify::{SharedWatcher, Subscription, WatchEvent},
};
use anyhow::{Context, Result, bail};
use nix::unistd::Pid;
use std::{
    fs::File as StdFile,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};
use tokio::{
    sync::mpsc::{Receiver, channel},
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

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

/// The memory events of a cgroup v2 leaf and of the cgroups above it.
struct CgroupV2 {
    leaf: PathBuf,
    events: MemoryEvents,

    /// The cgroup which merely wraps the leaf, crun places the process in
    /// a leaf below the container cgroup and the OOMs end up recorded in
    /// that parent. Only follow the parent in that layout. A pod cgroup
    /// shared with sibling containers (runc) must not be watched, otherwise
    /// an OOM of a sibling would be reported for this container. Prefer
    /// memory.events.local, the counters in memory.events include events
    /// from below, and fall back on kernels without the local file.
    parent: Option<MemoryEvents>,

    /// The hierarchical memory events of the pod cgroup, which outlives a
    /// leaf the systemd cgroup manager removes, or of the parent without
    /// one, and the OOM counters which the leaf does not account for. They
    /// advance while the leaf exists, so that only kills after its removal
    /// stay ambiguous.
    outer: Option<(PathBuf, OomCounters)>,
}

impl CgroupV2 {
    /// How many levels above the leaf the pod cgroup may be: its parent with
    /// runc, its grandparent with crun, which adds a leaf below the
    /// container cgroup.
    const POD_LEVELS: usize = 2;

    fn new(leaf: &Path) -> Self {
        let outer = Self::pod_cgroup(leaf)
            .or_else(|| leaf.parent())
            .and_then(|outer| {
                let path = outer.join("memory.events");
                match OOMWatcher::read_oom_counters(&path) {
                    Ok(counters) => Some((path, counters)),
                    Err(e) => {
                        debug!("No OOM baseline of the outer cgroup: {e:#}");
                        None
                    }
                }
            });
        Self {
            leaf: leaf.to_path_buf(),
            events: MemoryEvents::new(leaf.join("memory.events")),
            parent: OOMWatcher::wrapper_events_path(leaf).map(MemoryEvents::new),
            outer,
        }
    }

    /// Whether a watch event concerns the outer cgroup. Events which the
    /// kernel dropped may have, the baseline gets read again then.
    fn is_outer_event(&self, event: &WatchEvent) -> bool {
        self.outer.as_ref().is_some_and(|(path, _)| {
            event
                .path
                .as_deref()
                .is_none_or(|event_path| event_path == path)
        })
    }

    /// The pod cgroup above the leaf, if there is one within
    /// [`CgroupV2::POD_LEVELS`]. Further up, like in a QoS cgroup, the
    /// siblings are other pods.
    fn pod_cgroup(leaf: &Path) -> Option<&Path> {
        leaf.ancestors()
            .skip(1)
            .take(Self::POD_LEVELS)
            .find(|ancestor| {
                ancestor
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(Self::is_pod_cgroup_name)
            })
    }

    /// Whether the name is the one the kubelet gives to a pod cgroup, like
    /// `pod<uid>` with the cgroupfs driver and
    /// `kubepods-burstable-pod<uid>.slice` with the systemd one.
    fn is_pod_cgroup_name(name: &str) -> bool {
        let is_pod = |name: &str| {
            name.strip_prefix("pod").is_some_and(|uid| {
                !uid.is_empty()
                    && uid
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() || c == '-' || c == '_')
            })
        };
        match name.strip_suffix(".slice") {
            Some(slice) => slice.rsplit('-').next().is_some_and(is_pod),
            None => is_pod(name),
        }
    }

    /// Whether the counters of the leaf or its wrapper went up. After an
    /// event of the outer cgroup, its kills since the last check stem from
    /// a sibling otherwise, as long as the leaf still exists. The kernel
    /// counts a kill in the leaf before its ancestors, so the outer cgroup
    /// gets read first.
    fn check(&mut self, outer_changed: bool) -> bool {
        let outer = self
            .outer
            .as_ref()
            .filter(|_| outer_changed)
            .and_then(|(path, _)| OOMWatcher::read_oom_counters(path).ok());
        if OOMWatcher::check_cgroups(&self.leaf, &mut self.events, self.parent.as_mut()) {
            return true;
        }
        if let (Some(counters), Some((_, baseline))) = (outer, self.outer.as_mut())
            && !OOMWatcher::file_not_found(&self.events.path)
        {
            *baseline = counters;
        }
        false
    }

    /// Check for an OOM a last time once the process exited. If the leaf is
    /// gone, a kill of the outer cgroup since tells that it may have been
    /// one, and so does an outer cgroup which is gone as well.
    fn check_on_exit(&mut self) -> OOMEvent {
        debug!("Last resort check for OOM");
        if self.check(false) {
            return OOMEvent::new(true);
        }
        if !OOMWatcher::file_not_found(&self.events.path) {
            return OOMEvent::default();
        }
        let Some((path, baseline)) = &self.outer else {
            debug!("Leaf cgroup gone without outer counters");
            return OOMEvent::possible(true);
        };
        match OOMWatcher::read_oom_counters(path) {
            Ok(counters) => {
                debug!(
                    "Leaf cgroup gone, OOM kills of {}: {} since {}",
                    path.display(),
                    counters.oom_kill,
                    baseline.oom_kill
                );
                OOMEvent::possible(counters.oom_kill > baseline.oom_kill)
            }
            Err(e) => {
                debug!("Unable to read outer OOM counters: {e:#}");
                OOMEvent::possible(true)
            }
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
    ///
    /// The cgroupfs read does not block, so it runs inline instead of adding a
    /// blocking task to every exit of a container or exec.
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
    rx: Receiver<WatchEvent>,
    subscription: Subscription,
    _parent_subscription: Option<Subscription>,
    _pod_subscription: Option<Subscription>,
}

impl Subscriptions {
    /// Whether the reader of the shared watcher stopped, which ended the
    /// watches.
    fn watcher_failed(&self) -> bool {
        self.subscription.watcher_failed()
    }
}

impl OOMWatcher {
    pub(super) async fn oom_handling_cgroup_v2(
        token: &CancellationToken,
        pid: Pid,
        exit_paths: &[PathBuf],
    ) -> Result<Option<OOMEvent>> {
        // The setup reads procfs and cgroupfs inline, which does not block,
        // instead of adding a blocking task to every container and exec.
        let Some(subsystem_path) = Self::process_cgroup_subsystem_path_cgroup_v2(pid)
            .context("process cgroup subsystem path")?
        else {
            debug!("Stopping OOM handler because no cgroup subsystem path exists");
            return Ok(None);
        };

        debug!(
            "Using subsystem cgroup v2 path: {}",
            subsystem_path.display()
        );

        let event = Self::watch_cgroup_v2(token, &subsystem_path).await;
        if event.oom {
            debug!("Writing OOM files");
            Self::write_oom_files(exit_paths).await;
        }
        debug!("Done watching for ooms");
        Ok(Some(event))
    }

    /// Wait for an OOM of the cgroup v2 `leaf` until `token` gets cancelled
    /// and return what was found.
    ///
    /// An OOM counts once the counters of the leaf, or of the cgroup which
    /// merely wraps it, went up. The leaf may be gone at the exit, the
    /// systemd cgroup manager removes the scope of a container once its
    /// processes exited. Then the hierarchical `oom_kill` counter of the
    /// nearest ancestor which outlives it tells whether a kill happened
    /// meanwhile, see [`OOMEvent::possible_oom`]. A removal is no OOM
    /// on its own. cgroupfs reports it with `IN_IGNORED` only, which the
    /// watcher drops, the remove event shows up for other file systems.
    async fn watch_cgroup_v2(token: &CancellationToken, leaf: &Path) -> OOMEvent {
        let mut cgroup = CgroupV2::new(leaf);

        let path = cgroup.events.path.to_str();
        debug!(path, "Setup cgroup v2 handling");

        let mut subscriptions = match Self::subscribe(
            &cgroup.events,
            cgroup.parent.as_ref(),
            cgroup.outer.as_ref().map(|(path, _)| path.as_path()),
        ) {
            Ok(subscriptions) => subscriptions,
            // The cgroup may be gone already. Still check for an OOM once the
            // process exited.
            Err(e) => {
                debug!("Unable to watch memory events, checking on exit only: {e:#}");
                token.cancelled().await;
                return cgroup.check_on_exit();
            }
        };
        #[cfg(test)]
        tests::progress(leaf).send_modify(|p| p.subscribed += 1);

        let mut restarts = 0;
        let mut next_check = Instant::now();
        loop {
            tokio::select! {
                // Memory events may change all the time, for example while
                // processes left in the cgroup hit memory.high, which must
                // not hold back the exit. The check on exit reads the
                // counters of an OOM which ended the process anyway.
                biased;
                () = token.cancelled() => {
                    debug!("Loop cancelled");
                    return cgroup.check_on_exit();
                }
                event = subscriptions.rx.recv() => {
                    let Some(event) = event else {
                        // All watches ended, their files got removed, or the
                        // watcher stopped reading events.
                        let path = cgroup.events.path.to_str();
                        if !subscriptions.watcher_failed() || restarts >= Self::MAX_WATCHER_RESTARTS {
                            debug!(path, "Watches ended, checking once the process exited");
                            break;
                        }
                        restarts += 1;
                        warn!(path, "Watcher of the memory events stopped, watching again");
                        match Self::subscribe(
                            &cgroup.events,
                            cgroup.parent.as_ref(),
                            cgroup.outer.as_ref().map(|(path, _)| path.as_path()),
                        ) {
                            Ok(new) => subscriptions = new,
                            Err(e) => {
                                warn!(path, "Unable to watch memory events again, checking on exit only: {e:#}");
                                break;
                            }
                        }
                        #[cfg(test)]
                        tests::progress(leaf).send_modify(|p| p.subscribed += 1);
                        // The changes while nothing got watched, like the
                        // kills of siblings, refresh the baselines.
                        if cgroup.check(true) {
                            return OOMEvent::new(true);
                        }
                        continue;
                    };
                    debug!("Got OOM file event: {:?}", event);

                    // The kernel notifies a change of the memory events of
                    // a cgroup up to every 10 ms, and one of the hierarchical
                    // ones of every ancestor. The counters get read at most
                    // every `MIN_CHECK_INTERVAL`, for all events which came
                    // meanwhile.
                    tokio::select! {
                        biased;
                        () = token.cancelled() => {
                            debug!("Loop cancelled");
                            return cgroup.check_on_exit();
                        }
                        () = time::sleep_until(next_check) => {}
                    }
                    let (mut removed, mut outer) = (event.removed, cgroup.is_outer_event(&event));
                    while let Ok(event) = subscriptions.rx.try_recv() {
                        removed |= event.removed;
                        outer |= cgroup.is_outer_event(&event);
                    }

                    // Also with a removal among them, the counters of a leaf
                    // which is still there tell an OOM for sure.
                    next_check = Instant::now() + Self::MIN_CHECK_INTERVAL;
                    let oom = cgroup.check(outer);
                    #[cfg(test)]
                    tests::progress(leaf).send_modify(|p| {
                        p.handled += 1;
                        p.outer = cgroup.outer.as_ref().map(|(_, counters)| *counters);
                    });
                    if oom {
                        return OOMEvent::new(true);
                    }
                    if removed {
                        debug!("Got remove event, checking once the process exited");
                        break;
                    }
                    if Self::file_not_found(&cgroup.events.path) {
                        debug!("Memory events file gone, checking once the process exited");
                        break;
                    }
                    debug!("No OOM found in event");
                }
            }
        }

        drop(subscriptions);
        #[cfg(test)]
        tests::progress(leaf).send_modify(|p| p.ended = true);
        token.cancelled().await;
        cgroup.check_on_exit()
    }

    /// How often an OOM watcher watches its files again on a new shared
    /// watcher, after the reader of the events failed.
    const MAX_WATCHER_RESTARTS: u32 = 3;

    /// How often a cgroup v2 watcher reads the counters at most, see
    /// [`OOMWatcher::watch_cgroup_v2`]. Under memory pressure, like with
    /// `memory.high`, the events of every container of a pod would be read
    /// up to a hundred times a second otherwise.
    const MIN_CHECK_INTERVAL: Duration = Duration::from_millis(100);

    /// Watch the memory events of the container and, if present, the ones of
    /// its wrapper cgroup.
    fn subscribe(
        events: &MemoryEvents,
        parent_events: Option<&MemoryEvents>,
        pod_events: Option<&Path>,
    ) -> Result<Subscriptions> {
        let watcher = SharedWatcher::get()?;
        let (events_tx, rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let subscription = watcher
            .subscribe(&events.path, events_tx.clone())
            .context("watch memory events file")?;

        let mut parent_subscription = None;
        if let Some(parent) = parent_events {
            debug!(
                "Also watching parent memory events: {}",
                parent.path.display()
            );
            match watcher.subscribe(&parent.path, events_tx.clone()) {
                Ok(subscription) => parent_subscription = Some(subscription),
                Err(e) => debug!("Could not watch parent memory events: {e:#}"),
            }
        }

        // The kills of siblings advance the baseline of the outer cgroup.
        let mut pod_subscription = None;
        if let Some(pod) = pod_events {
            debug!("Also watching outer memory events: {}", pod.display());
            match watcher.subscribe(pod, events_tx) {
                Ok(subscription) => pod_subscription = Some(subscription),
                Err(e) => debug!("Could not watch pod memory events: {e:#}"),
            }
        }

        Ok(Subscriptions {
            rx,
            subscription,
            _parent_subscription: parent_subscription,
            _pod_subscription: pod_subscription,
        })
    }

    /// Check the memory events of the container and of its wrapper cgroup for
    /// new OOMs. An OOM of the parent only counts while it wraps the container
    /// alone, otherwise it may stem from a sibling.
    fn check_cgroups(
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

    /// The memory events file of the parent of `leaf`, if it merely wraps it.
    fn wrapper_events_path(leaf: &Path) -> Option<PathBuf> {
        leaf.parent()
            .filter(|_| Self::is_only_child(leaf))
            .and_then(|parent| {
                [
                    parent.join("memory.events.local"),
                    parent.join("memory.events"),
                ]
                .into_iter()
                .find(|path| path.exists())
            })
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
        let mut counters = OomCounters::default();
        let mut has_oom = false;

        for line in BufReader::new(fp).lines() {
            let line = line.context("read line from memory events file")?;
            trace!(line);

            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            let target = match key {
                "oom" => {
                    has_oom = true;
                    &mut counters.oom
                }
                "oom_kill" => &mut counters.oom_kill,
                _ => continue,
            };
            *target = value.trim_end().parse().context("parse u64 counter")?;
        }

        // Every memory events file has it, also of kernels without the kill
        // counter. Without, it is no memory events file, or one which is
        // being written, like a fake one.
        if !has_oom {
            bail!("no oom counter in memory events file {}", path.display())
        }
        Ok(counters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex, PoisonError},
    };
    use tempfile::NamedTempFile;
    use tokio::{
        sync::watch,
        task,
        time::{Duration, timeout},
    };

    /// How far the watch of a cgroup v2 leaf got, which the tests wait for
    /// instead of sleeping.
    #[derive(Clone, Copy, Debug, Default)]
    pub(super) struct Progress {
        /// How often its files got watched.
        pub(super) subscribed: usize,

        /// How many batches of events got handled.
        pub(super) handled: usize,

        /// The counters of the outer cgroup after the last batch.
        pub(super) outer: Option<OomCounters>,

        /// Whether it stopped watching and waits for the exit.
        pub(super) ended: bool,
    }

    static PROGRESS: Mutex<Vec<(PathBuf, Arc<watch::Sender<Progress>>)>> = Mutex::new(Vec::new());

    /// The progress of the watch of a leaf, see [`Progress`].
    pub(super) fn progress(leaf: &Path) -> Arc<watch::Sender<Progress>> {
        let mut all = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, progress)) = all.iter().find(|(path, _)| path == leaf) {
            return progress.clone();
        }
        let progress = Arc::new(watch::channel(Progress::default()).0);
        all.push((leaf.to_path_buf(), progress.clone()));
        progress
    }

    /// Wait until the watch of the leaf got as far as `f` tells.
    async fn wait_progress(leaf: &Path, f: impl Fn(&Progress) -> bool) -> Result<()> {
        let mut rx = progress(leaf).subscribe();
        timeout(Duration::from_secs(5), rx.wait_for(f))
            .await
            .context("watch did not get as far")??;
        Ok(())
    }

    /// Wait until the watch of the leaf took the outer cgroup at `oom_kill`
    /// kills as baseline.
    async fn outer_kills_seen(leaf: &Path, oom_kill: u64) -> Result<()> {
        wait_progress(leaf, |p| p.outer.is_some_and(|c| c.oom_kill == oom_kill)).await
    }

    #[tokio::test]
    async fn cgroup_v2_removal_by_cgroupfs_is_no_oom() -> Result<()> {
        let root = tempfile::tempdir()?;
        let [_, pod, scope, leaf] = fake_pod(root.path(), false)?;
        let (token, task) = watch_v2(&leaf);
        baselines_taken(&leaf).await?;

        // cgroupfs ends the watches of the files of a removed cgroup with
        // IN_IGNORED only, there is no event of the file itself.
        let watcher = SharedWatcher::get()?;
        let events = leaf.join("memory.events");
        watcher.remove_watch(&events)?;
        timeout(Duration::from_secs(5), async {
            while watcher.watches(&events) {
                task::yield_now().await;
            }
        })
        .await?;
        std::fs::remove_dir_all(&scope)?;
        assert!(!task.is_finished());

        // The pod is still watched, a kill there after the removal may have
        // been the one of the container.
        write_events(&pod, 0, 4)?;
        wait_progress(&leaf, |p| p.ended).await?;
        token.cancel();
        assert_eq!(
            timeout(Duration::from_secs(5), task).await??,
            OOMEvent::possible(true)
        );
        Ok(())
    }

    /// Stop the reader of the shared watcher of the leaf, once the watch task
    /// watches it, and wait until the task watches it again on a new one.
    async fn stop_watcher(leaf: &Path) -> Result<()> {
        baselines_taken(leaf).await?;
        let failed = SharedWatcher::get()?;
        failed.stop();
        wait_progress(leaf, |p| p.subscribed >= 2).await?;
        assert!(!Arc::ptr_eq(&SharedWatcher::get()?, &failed));
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v2_watches_again_after_the_watcher_stopped() -> Result<()> {
        // An OOM of the container after the failure still gets found.
        let root = tempfile::tempdir()?;
        let [_, _, _, leaf] = fake_pod(root.path(), false)?;
        let (token, task) = watch_v2(&leaf);
        stop_watcher(&leaf).await?;
        write_events(&leaf, 1, 4)?;
        let event = timeout(Duration::from_secs(5), task).await??;
        assert_eq!(event, OOMEvent::new(true));
        token.cancel();

        // The kill of a sibling after the failure is no possible OOM of the
        // container, whose scope got removed.
        let root = tempfile::tempdir()?;
        let [_, pod, scope, leaf] = fake_pod(root.path(), false)?;
        let (token, task) = watch_v2(&leaf);
        stop_watcher(&leaf).await?;
        write_events(&pod, 0, 4)?;
        outer_kills_seen(&leaf, 4).await?;
        std::fs::remove_dir_all(&scope)?;
        token.cancel();
        let event = timeout(Duration::from_secs(5), task).await??;
        assert_eq!(event, OOMEvent::default());
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
        let mut check = || OOMWatcher::check_cgroups(&leaf, &mut events, Some(&mut parent_events));

        // An OOM recorded in the wrapper is found although the leaf is unchanged.
        std::fs::write(&parent_file, "oom 1\noom_kill 1\n")?;
        assert!(check());
        assert!(!check());

        // While a sibling exists, its OOMs are not attributed to this container.
        let sibling = parent.path().join("sibling");
        std::fs::create_dir(&sibling)?;
        std::fs::write(&parent_file, "oom 2\noom_kill 2\n")?;
        assert!(!check());

        // Once it is gone again, neither are its earlier OOMs, only new ones.
        std::fs::remove_dir(&sibling)?;
        assert!(!check());
        std::fs::write(&parent_file, "oom 3\noom_kill 3\n")?;
        assert!(check());
        Ok(())
    }

    /// Watch a fake cgroup v2 leaf in a task of its own.
    fn watch_v2(leaf: &Path) -> (CancellationToken, task::JoinHandle<OOMEvent>) {
        let token = CancellationToken::new();
        let (stop, leaf) = (token.clone(), leaf.to_path_buf());
        let task = task::spawn(async move { OOMWatcher::watch_cgroup_v2(&stop, &leaf).await });
        (token, task)
    }

    /// Let the watch task of `leaf` take its baselines and watch its files.
    /// A task which starts late would take changes as its baselines
    /// otherwise.
    async fn baselines_taken(leaf: &Path) -> Result<()> {
        wait_progress(leaf, |p| p.subscribed >= 1).await
    }

    fn write_events(dir: &Path, oom: u64, oom_kill: u64) -> Result<()> {
        std::fs::write(
            dir.join("memory.events"),
            format!("low 0\nhigh 0\nmax 0\noom {oom}\noom_kill {oom_kill}\n"),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v2_event_of_leaf_is_oom() -> Result<()> {
        let pod = tempfile::tempdir()?;
        let leaf = pod.path().join("ctr");
        std::fs::create_dir(&leaf)?;
        write_events(&leaf, 0, 0)?;
        let (_token, task) = watch_v2(&leaf);
        baselines_taken(&leaf).await?;

        write_events(&leaf, 1, 1)?;
        let event = timeout(Duration::from_secs(5), task).await??;
        assert_eq!(event, OOMEvent::new(true));
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v2_unchanged_leaf_is_no_oom() -> Result<()> {
        let pod = tempfile::tempdir()?;
        let leaf = pod.path().join("ctr");
        std::fs::create_dir_all(&leaf)?;
        std::fs::create_dir(pod.path().join("sibling"))?;
        write_events(&leaf, 0, 0)?;
        write_events(pod.path(), 0, 0)?;
        let (token, task) = watch_v2(&leaf);
        baselines_taken(&leaf).await?;

        // A kill of a sibling while the leaf exists does not count.
        write_events(pod.path(), 1, 1)?;
        token.cancel();
        assert_eq!(
            timeout(Duration::from_secs(5), task).await??,
            OOMEvent::default()
        );
        Ok(())
    }

    /// A pod cgroup of the systemd driver with a sibling container and the
    /// scope of a container, which holds the leaf with crun. Returns the
    /// QoS and pod cgroups, the scope and the leaf.
    fn fake_pod(root: &Path, crun: bool) -> Result<[PathBuf; 4]> {
        let qos = root.join("kubepods-burstable.slice");
        let pod = qos.join("kubepods-burstable-pod12ab_34cd.slice");
        let scope = pod.join("crio-ctr.scope");
        let leaf = if crun {
            scope.join("container")
        } else {
            scope.clone()
        };
        std::fs::create_dir_all(&leaf)?;
        std::fs::create_dir(pod.join("crio-sibling.scope"))?;
        for dir in [&qos, &pod, &scope, &leaf] {
            write_events(dir, 0, 3)?;
        }
        Ok([qos, pod, scope, leaf])
    }

    /// Watch the leaf, let a sibling get killed before if `sibling_kill`,
    /// remove the scope, let `after_removal` count kills and return what the
    /// watch found on exit.
    async fn removed_scope(
        crun: bool,
        sibling_kill: bool,
        after_removal: impl Fn(&Path, &Path) -> Result<()>,
    ) -> Result<OOMEvent> {
        let root = tempfile::tempdir()?;
        let [qos, pod, scope, leaf] = fake_pod(root.path(), crun)?;
        let (token, task) = watch_v2(&leaf);
        baselines_taken(&leaf).await?;

        // The watch notices it while the leaf exists.
        if sibling_kill {
            write_events(&pod, 0, 4)?;
            outer_kills_seen(&leaf, 4).await?;
        }
        std::fs::remove_dir_all(&scope)?;
        after_removal(&qos, &pod)?;
        // Waiting for the exit, not taken for an OOM on its own.
        wait_progress(&leaf, |p| p.ended).await?;
        assert!(!task.is_finished());

        token.cancel();
        Ok(timeout(Duration::from_secs(5), task).await??)
    }

    #[tokio::test]
    async fn cgroup_v2_removed_leaf_falls_back_to_pod() -> Result<()> {
        let possible = OOMEvent {
            oom: false,
            possible_oom: true,
        };
        for crun in [false, true] {
            // A kill which the pod counted after the leaf went away.
            let event = removed_scope(crun, false, |_, pod| write_events(pod, 0, 4)).await?;
            assert_eq!(event, possible);
            assert!(event.is_oom(137));
            assert!(!event.is_oom(0));

            // Without one, it is no OOM.
            let event = removed_scope(crun, false, |_, _| Ok(())).await?;
            assert_eq!(event, OOMEvent::default());
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cgroup_v2_sibling_kills_do_not_count() -> Result<()> {
        for crun in [false, true] {
            // A sibling got killed while the leaf existed.
            let event = removed_scope(crun, true, |_, _| Ok(())).await?;
            assert_eq!(event, OOMEvent::default());
        }
        Ok(())
    }

    #[test]
    fn cgroup_v2_lost_events_refresh_the_outer_baseline() -> Result<()> {
        let root = tempfile::tempdir()?;
        let [_, pod, scope, leaf] = fake_pod(root.path(), false)?;
        let mut cgroup = CgroupV2::new(&leaf);

        // A sibling got killed while the kernel dropped the events.
        write_events(&pod, 0, 4)?;
        assert!(!cgroup.check(cgroup.is_outer_event(&WatchEvent::LOST)));

        // The removal of the leaf afterwards is no possible OOM then.
        std::fs::remove_dir_all(&scope)?;
        assert_eq!(cgroup.check_on_exit(), OOMEvent::default());
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v2_is_not_compared_above_the_pod() -> Result<()> {
        // The QoS cgroup has other pods.
        let event = removed_scope(false, false, |qos, _| write_events(qos, 0, 4)).await?;
        assert_eq!(event, OOMEvent::default());

        // Without the pod cgroup, nothing tells.
        let event = removed_scope(false, false, |_, pod| Ok(std::fs::remove_dir_all(pod)?)).await?;
        assert_eq!(event, OOMEvent::possible(true));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cgroup_v2_removed_leaf_without_pod_falls_back_to_parent() -> Result<()> {
        // A cgroup parent not created by the kubelet.
        let run = async |sibling_kill: bool, kill: bool, remove_parent: bool| -> Result<OOMEvent> {
            let root = tempfile::tempdir()?;
            let parent = root.path().join("custom.slice");
            let leaf = parent.join("ctr.scope");
            std::fs::create_dir_all(&leaf)?;
            std::fs::create_dir(parent.join("sibling.scope"))?;
            write_events(&parent, 0, 3)?;
            write_events(&leaf, 0, 3)?;
            let (token, task) = watch_v2(&leaf);
            baselines_taken(&leaf).await?;

            if sibling_kill {
                write_events(&parent, 0, 4)?;
                outer_kills_seen(&leaf, 4).await?;
            }
            std::fs::remove_dir_all(&leaf)?;
            if kill {
                write_events(&parent, 0, 5)?;
            }
            if remove_parent {
                std::fs::remove_dir_all(&parent)?;
            }
            token.cancel();
            Ok(timeout(Duration::from_secs(5), task).await??)
        };

        assert_eq!(run(false, true, false).await?, OOMEvent::possible(true));
        assert_eq!(run(false, false, false).await?, OOMEvent::default());
        assert_eq!(run(true, false, false).await?, OOMEvent::default());
        assert_eq!(run(false, false, true).await?, OOMEvent::possible(true));
        Ok(())
    }

    #[test]
    fn pod_cgroup_names() {
        for name in [
            "kubepods-burstable-pod12ab_34cd.slice",
            "kubepods-pod12ab_34cd.slice",
            "pod12ab-34cd",
        ] {
            assert!(CgroupV2::is_pod_cgroup_name(name), "{name}");
        }
        for name in [
            "kubepods.slice",
            "kubepods-burstable.slice",
            "crio-12ab.scope",
            "pod",
            "podman.slice",
            "libpod-12ab.scope",
        ] {
            assert!(!CgroupV2::is_pod_cgroup_name(name), "{name}");
        }
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
