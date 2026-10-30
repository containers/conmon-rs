//! The memory events of a cgroup v2 and of the cgroups above it.

use super::{
    OOMEvent, OOMWatcher, WATCH_EVENT_CHANNEL_SIZE,
    inotify::{SharedWatcher, Subscription, WatchEvent},
};
use anyhow::{Context, Result, bail};
use nix::unistd::Pid;
use std::path::{Path, PathBuf};
use tokio::{
    sync::mpsc::{Receiver, channel},
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

/// Tell the tests how far the watch of a leaf got, see `tests::Progress`.
/// Nothing outside of them.
macro_rules! progress {
    ($leaf:expr, |$p:ident| $update:expr) => {
        #[cfg(test)]
        tests::progress($leaf).send_modify(|$p| $update);
    };
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

    /// Whether the file has the kill counter, which kernels since 4.13 do.
    has_kill: bool,
}

impl OomCounters {
    /// Whether a process got killed since `baseline`. The kernel counts an
    /// `oom` before the OOM killer runs, which may kill nothing, if reclaim
    /// freed enough memory meanwhile or a victim is exiting already, so only
    /// kernels without the kill counter fall back to it. The cgroup v1
    /// watcher waits for a kill as well.
    fn increased_since(&self, baseline: &Self) -> bool {
        if self.has_kill {
            self.oom_kill > baseline.oom_kill
        } else {
            self.oom > baseline.oom
        }
    }
}

/// The memory events of a cgroup v2 leaf and of the cgroups above it.
///
/// crun places the process in a leaf below the container cgroup and applies
/// the memory limit to both. The kill is always counted in the victim's own
/// cgroup, the leaf, so it tells about an OOM which the limit of the cgroup
/// wrapping it caused as well.
struct CgroupV2 {
    events: MemoryEvents,

    /// The hierarchical memory events of the pod cgroup, which outlives a
    /// leaf the systemd cgroup manager removes, or without one of the
    /// nearest ancestor which is no scope, and the OOM counters which the
    /// leaf does not account for. They advance while the leaf exists, so
    /// that only kills after its removal stay ambiguous.
    outer: Option<(PathBuf, OomCounters)>,
}

impl CgroupV2 {
    /// How many levels above the leaf the pod cgroup may be: its parent with
    /// runc, its grandparent with crun, which adds a leaf below the
    /// container cgroup.
    const POD_LEVELS: usize = 2;

    fn new(leaf: &Path) -> Self {
        let outer = Self::pod_cgroup(leaf)
            .or_else(|| Self::outliving_ancestor(leaf))
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
            events: MemoryEvents::new(leaf.join("memory.events")),
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

    /// The nearest ancestor which systemd does not remove together with the
    /// leaf: the parent, unless the leaf is the one crun creates below the
    /// scope of the container (`<parent>/crio-<id>.scope/container`).
    fn outliving_ancestor(leaf: &Path) -> Option<&Path> {
        leaf.ancestors()
            .skip(1)
            .find(|ancestor| ancestor.extension() != Some("scope".as_ref()))
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

    /// Whether the counters of the leaf went up. After an event of the outer
    /// cgroup, its kills since the last check stem from a sibling otherwise,
    /// as long as the leaf still exists. The kernel counts a kill in the leaf
    /// before its ancestors, so the outer cgroup gets read first.
    fn check(&mut self, outer_changed: bool) -> bool {
        let outer = self
            .outer
            .as_ref()
            .filter(|_| outer_changed)
            .and_then(|(path, _)| OOMWatcher::read_oom_counters(path).ok());
        if self.events.check() {
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
    /// one, and so does an outer cgroup which is gone as well. A leaf which
    /// exists without memory events has no memory controller, its kills
    /// count in the outer cgroup, but without outer counters nothing tells
    /// of an OOM.
    ///
    /// For such a leaf, the outer counters include the kills of siblings
    /// over the whole life of the leaf, the baseline only advances while the
    /// leaf has memory events, so an OOM of a sibling may still read as one
    /// of this container for an exit with 137.
    fn check_on_exit(&mut self) -> OOMEvent {
        debug!("Last resort check for OOM");
        if self.check(false) {
            return OOMEvent::new(true);
        }
        if !OOMWatcher::file_not_found(&self.events.path) {
            return OOMEvent::default();
        }
        let leaf_gone = self
            .events
            .path
            .parent()
            .is_none_or(OOMWatcher::file_not_found);
        let Some((path, baseline)) = &self.outer else {
            debug!(
                leaf_gone,
                "Leaf cgroup without memory events or outer counters"
            );
            return OOMEvent::possible(leaf_gone);
        };
        match OOMWatcher::read_oom_counters(path) {
            Ok(counters) => {
                debug!(
                    leaf_gone,
                    "No leaf memory events, OOM counters of {}: {counters:?} since {baseline:?}",
                    path.display(),
                );
                OOMEvent::possible(counters.increased_since(baseline))
            }
            Err(e) => {
                debug!(leaf_gone, "Unable to read outer OOM counters: {e:#}");
                OOMEvent::possible(leaf_gone)
            }
        }
    }

    /// Watch the memory events of the container and, if present, the ones of
    /// the outer cgroup.
    fn subscribe(&self) -> Result<Subscriptions> {
        let watcher = SharedWatcher::get()?;
        let (events_tx, rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        let subscription = watcher
            .subscribe(&self.events.path, events_tx.clone())
            .context("watch memory events file")?;

        // The kills of siblings advance the baseline of the outer cgroup.
        let mut pod_subscription = None;
        if let Some((pod, _)) = &self.outer {
            debug!("Also watching outer memory events: {}", pod.display());
            match watcher.subscribe(pod, events_tx) {
                Ok(subscription) => pod_subscription = Some(subscription),
                Err(e) => debug!("Could not watch pod memory events: {e:#}"),
            }
        }

        Ok(Subscriptions {
            rx,
            subscription,
            _pod_subscription: pod_subscription,
        })
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

        Ok(Some(Self::watch_cgroup_v2(token, &subsystem_path).await))
    }

    /// Wait for an OOM of the cgroup v2 `leaf` until `token` gets cancelled
    /// and return what was found.
    ///
    /// An OOM counts once the counters of the leaf went up. The leaf may be
    /// gone at the exit, the systemd cgroup manager removes the scope of a
    /// container once its processes exited. Then the hierarchical `oom_kill`
    /// counter of the nearest ancestor which outlives it tells whether a kill
    /// happened meanwhile, see [`OOMEvent::possible_oom`]. A removal is no OOM
    /// on its own. cgroupfs reports it with `IN_IGNORED` only, which ends the
    /// watch without an event.
    async fn watch_cgroup_v2(token: &CancellationToken, leaf: &Path) -> OOMEvent {
        let mut cgroup = CgroupV2::new(leaf);

        let path = cgroup.events.path.to_str();
        debug!(path, "Setup cgroup v2 handling");

        let mut subscriptions = match cgroup.subscribe() {
            Ok(subscriptions) => subscriptions,
            // The cgroup may be gone already. Still check for an OOM once the
            // process exited.
            Err(e) => {
                debug!("Unable to watch memory events, checking on exit only: {e:#}");
                token.cancelled().await;
                return cgroup.check_on_exit();
            }
        };
        progress!(leaf, |p| p.subscribed += 1);

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
                        match cgroup.subscribe() {
                            Ok(new) => subscriptions = new,
                            Err(e) => {
                                warn!(path, "Unable to watch memory events again, checking on exit only: {e:#}");
                                break;
                            }
                        }
                        progress!(leaf, |p| p.subscribed += 1);
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
                    let mut outer = cgroup.is_outer_event(&event);
                    while let Ok(event) = subscriptions.rx.try_recv() {
                        outer |= cgroup.is_outer_event(&event);
                    }

                    next_check = Instant::now() + Self::MIN_CHECK_INTERVAL;
                    let oom = cgroup.check(outer);
                    progress!(leaf, |p| {
                        p.handled += 1;
                        p.outer = cgroup.outer.as_ref().map(|(_, counters)| *counters);
                    });
                    if oom {
                        return OOMEvent::new(true);
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
        progress!(leaf, |p| p.ended = true);
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
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("read memory events file: {}", path.display()))?;
        trace!(content);
        let mut counters = OomCounters::default();
        let mut has_oom = false;

        for (key, value) in Self::counters(&content) {
            let target = match key {
                "oom" => {
                    has_oom = true;
                    &mut counters.oom
                }
                "oom_kill" => {
                    counters.has_kill = true;
                    &mut counters.oom_kill
                }
                _ => continue,
            };
            *target = value.context("parse u64 counter")?;
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
mod tests;
