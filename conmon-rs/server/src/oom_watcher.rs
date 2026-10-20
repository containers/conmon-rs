use crate::mutex::lock;
use anyhow::{Context, Result, bail};
use nix::sys::{
    signal::Signal,
    statfs::{FsType, statfs},
};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode},
};
use std::{
    collections::HashMap,
    fs::File as StdFile,
    io::{self, BufRead, BufReader},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ErrorKind},
    runtime::Handle,
    sync::mpsc::{Receiver, Sender, channel},
    task,
    time::{self, Duration},
};
use tokio_eventfd::EventFd;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, trace};

pub const CGROUP2_SUPER_MAGIC: FsType = FsType(libc::CGROUP2_SUPER_MAGIC as _);

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

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

#[derive(Debug, Default, PartialEq, Eq)]
pub struct OOMEvent {
    pub oom: bool,

    /// The cgroup of the process got removed, which hides whether an OOM
    /// happened, but there is a hint which may also stem from a sibling:
    /// with cgroup v1, an event before the removal, with cgroup v2, a new
    /// OOM kill of an outer cgroup or none to tell. So it only counts for a
    /// process which got killed by SIGKILL, see [`OOMEvent::is_oom`].
    pub possible_oom: bool,
}

impl OOMEvent {
    fn new(oom: bool) -> Self {
        Self {
            oom,
            ..Self::default()
        }
    }

    /// A possible OOM if `possible`, none otherwise.
    pub(crate) fn possible(possible: bool) -> Self {
        Self {
            oom: false,
            possible_oom: possible,
        }
    }

    /// Whether the process which exited with `exit_code` got OOM killed.
    pub fn is_oom(&self, exit_code: i32) -> bool {
        self.oom || (self.possible_oom && exit_code == 128 + Signal::SIGKILL as i32)
    }
}

/// The state of a cgroup v1 `memory.oom_control` file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OomControl {
    /// Whether the OOM killer is disabled, tasks wait in the OOM then.
    kill_disable: bool,

    /// Whether the cgroup is in an OOM right now.
    under_oom: bool,

    /// Processes killed in this cgroup, kernels before 4.13 have no counter.
    oom_kill: Option<u64>,
}

impl OomControl {
    fn parse(content: &str) -> Self {
        let mut control = Self::default();
        for line in content.lines() {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            let value = value.trim().parse::<u64>().ok();
            match key {
                "oom_kill_disable" => control.kill_disable = value.is_some_and(|v| v != 0),
                "under_oom" => control.under_oom = value.is_some_and(|v| v != 0),
                "oom_kill" => control.oom_kill = value,
                _ => {}
            }
        }
        control
    }
}

/// The outcome of confirming a cgroup v1 OOM event.
#[derive(Debug, PartialEq, Eq)]
enum Confirmation {
    Oom,
    /// The cgroup was already gone at the event, which removal signals.
    Removed,
    /// The cgroup went away while confirming.
    Gone,
    /// No kill showed up while the cgroup exists.
    Unconfirmed,
    /// The process exited while confirming.
    Exited,
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
        // The setup reads procfs and cgroupfs inline, which does not block,
        // instead of adding a blocking task to every container and exec.
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

        // Taken before registering, an OOM in between is found by the check on
        // exit. Unknown without the counter or if it cannot be read, then
        // every event counts.
        let baseline = match Self::read_oom_control(&memory_cgroup_file_oom_path) {
            Ok(control) => control.oom_kill,
            Err(e) => {
                debug!("Could not read initial OOM kills: {:#}", e);
                None
            }
        };

        let oom_cgroup_file = OpenOptions::new()
            .write(true)
            .open(&memory_cgroup_file_oom_path)
            .await
            .context("opening cgroup oom file")?;
        let oom_event_fd = EventFd::new(0, false).context("creating eventfd")?;

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

        let event =
            Self::watch_cgroup_v1(&token, oom_event_fd, &memory_cgroup_file_oom_path, baseline)
                .await;
        if event.oom {
            Self::write_oom_files(exit_paths).await;
        }
        if let Err(e) = tx.try_send(event) {
            error!("try_send failed: {:#}", e);
        }

        debug!("Done watching for ooms");
        Ok(())
    }

    /// Wait for an OOM of a cgroup v1 memory controller until `token` gets
    /// cancelled and return what was found.
    ///
    /// The eventfd also fires for an OOM of an ancestor cgroup, whose victim
    /// may be a sibling container, and once when the cgroup gets removed. So
    /// an event gets confirmed by the `oom_kill` counter of the cgroup. The
    /// kernel signals the eventfd before it kills, so the counter is read
    /// again a few times. It is not hierarchical either, a victim in a memcg
    /// nested inside the container is not counted, which `under_oom` covers.
    /// A missed OOM is worse than a rare false one, like conmon: an event
    /// counts without a known baseline or counter, and if the process exits
    /// while the event gets confirmed and the cgroup is still under OOM. An
    /// event which stayed unconfirmed, like an OOM of a sibling, does not
    /// count later, a normal stop would be taken for an OOM otherwise. If the
    /// cgroup goes away after an event, it may have been an OOM which killed
    /// the last process or the one of a sibling followed by a normal exit,
    /// see [`OOMEvent::possible_oom`]. The removal alone signals once.
    async fn watch_cgroup_v1(
        token: &CancellationToken,
        mut event_fd: impl AsyncRead + Unpin,
        oom_control: &Path,
        baseline: Option<u64>,
    ) -> OOMEvent {
        let mut buffer = [0u8; 8];
        loop {
            tokio::select! {
                // An OOM kill often ends the process, which cancels the token.
                // Handle the event first then.
                biased;
                res = event_fd.read(&mut buffer) => {
                    let count = match res {
                        Ok(n) if n == buffer.len() => u64::from_ne_bytes(buffer),
                        res => {
                            debug!("Unable to read OOM eventfd, checking on exit only: {res:?}");
                            break;
                        }
                    };
                    debug!("Got OOM event, count {count}");
                    match Self::confirm_oom_event(token, oom_control, baseline).await {
                        Confirmation::Oom => return OOMEvent::new(true),
                        Confirmation::Removed => {
                            // Removing the cgroup signals the eventfd once. A
                            // count above that means another event came
                            // before.
                            debug!("Memory cgroup removed");
                            return OOMEvent::possible(count > 1);
                        }
                        Confirmation::Gone => {
                            debug!("Memory cgroup removed while confirming the event");
                            return OOMEvent::possible(true);
                        }
                        Confirmation::Unconfirmed => {
                            debug!("OOM event without a kill in this cgroup");
                        }
                        Confirmation::Exited => {
                            return Self::check_oom_on_exit(oom_control, baseline, true).await;
                        }
                    }
                }
                _ = token.cancelled() => {
                    debug!("Loop cancelled");
                    break;
                }
            }
        }
        token.cancelled().await;
        Self::check_oom_on_exit(oom_control, baseline, false).await
    }

    /// The delays between the reads which confirm an OOM event.
    const CONFIRM_BACKOFF: [Duration; 4] = [
        Duration::from_millis(10),
        Duration::from_millis(50),
        Duration::from_millis(250),
        Duration::from_secs(1),
    ];

    /// Confirm an eventfd event of a cgroup v1 memory controller, see
    /// [`OOMWatcher::watch_cgroup_v1`].
    async fn confirm_oom_event(
        token: &CancellationToken,
        oom_control: &Path,
        baseline: Option<u64>,
    ) -> Confirmation {
        let Some(baseline) = baseline else {
            debug!("No OOM kill baseline, taking the event as OOM");
            return Confirmation::Oom;
        };
        let mut delays = Self::CONFIRM_BACKOFF.into_iter();
        let mut first = true;
        loop {
            match Self::read_oom_control(oom_control) {
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    return if first {
                        Confirmation::Removed
                    } else {
                        Confirmation::Gone
                    };
                }
                Err(e) => {
                    debug!("Unable to read OOM kills, taking the event as OOM: {:#}", e);
                    return Confirmation::Oom;
                }
                Ok(control) => match control.oom_kill {
                    Some(kills) if kills > baseline => return Confirmation::Oom,
                    None => return Confirmation::Oom,
                    // Without the OOM killer the tasks wait in the OOM.
                    Some(_) if control.kill_disable && control.under_oom => {
                        return Confirmation::Oom;
                    }
                    Some(_) => match delays.next() {
                        Some(delay) => {
                            first = false;
                            tokio::select! {
                                _ = time::sleep(delay) => {}
                                // The counter is final once the process exited.
                                _ = token.cancelled() => return Confirmation::Exited,
                            }
                        }
                        // A victim in a nested memcg, which the counter of
                        // this cgroup does not count.
                        None if control.under_oom => return Confirmation::Oom,
                        None => return Confirmation::Unconfirmed,
                    },
                },
            }
        }
    }

    /// Check for an OOM kill a last time once the process exited. An event
    /// whose confirmation the exit interrupted counts if the cgroup is still
    /// under OOM, and may have been one if the cgroup is gone.
    async fn check_oom_on_exit(
        oom_control: &Path,
        baseline: Option<u64>,
        confirming: bool,
    ) -> OOMEvent {
        match Self::read_oom_control(oom_control) {
            // Without a baseline the counter tells nothing, it may stem from
            // before the container. An event took that into account already.
            Ok(control) => OOMEvent::new(
                baseline
                    .zip(control.oom_kill)
                    .is_some_and(|(baseline, kills)| kills > baseline)
                    || (confirming && control.under_oom),
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => OOMEvent::possible(confirming),
            Err(e) => {
                debug!("Unable to read OOM kills on exit: {:#}", e);
                OOMEvent::new(confirming)
            }
        }
    }

    /// Read a cgroup v1 `memory.oom_control` file. Inline like the other
    /// cgroupfs reads, they do not block.
    fn read_oom_control(path: &Path) -> io::Result<OomControl> {
        Ok(OomControl::parse(&std::fs::read_to_string(path)?))
    }

    async fn oom_handling_cgroup_v2(
        token: CancellationToken,
        pid: u32,
        exit_paths: &[PathBuf],
        tx: Sender<OOMEvent>,
    ) -> Result<()> {
        // The setup reads procfs and cgroupfs inline, which does not block,
        // instead of adding a blocking task to every container and exec.
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

        let event = Self::watch_cgroup_v2(&token, &subsystem_path).await;
        if event.oom {
            debug!("Writing OOM files");
            Self::write_oom_files(exit_paths).await;
        }
        if let Err(e) = tx.try_send(event) {
            error!("try_send failed: {:#}", e);
        }

        debug!("Done watching for ooms");
        Ok(())
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
    /// on its own. cgroupfs reports it with `IN_IGNORED` only, which notify
    /// drops, the remove event shows up for other file systems.
    async fn watch_cgroup_v2(token: &CancellationToken, leaf: &Path) -> OOMEvent {
        let mut cgroup = CgroupV2::new(leaf);

        let path = cgroup.events.path.to_str();
        debug!(path, "Setup cgroup v2 handling");

        // Adding the watches runs on the blocking pool. The process may exit
        // in the meantime, which must be reported without waiting for a busy
        // pool.
        let mut subscriptions = tokio::select! {
            _ = token.cancelled() => {
                debug!("Cancelled while adding the watches");
                return cgroup.check_on_exit();
            }
            res = Self::subscribe(
                &cgroup.events,
                cgroup.parent.as_ref(),
                cgroup.outer.as_ref().map(|(path, _)| path.as_path()),
            ) => match res {
                Ok(subscriptions) => subscriptions,
                // The cgroup may be gone already. Still check for an OOM once
                // the process exited.
                Err(e) => {
                    debug!("Unable to watch memory events, checking on exit only: {:#}", e);
                    token.cancelled().await;
                    return cgroup.check_on_exit();
                }
            },
        };

        loop {
            tokio::select! {
                // Memory events may change all the time, for example while
                // processes left in the cgroup hit memory.high, which must
                // not hold back the exit. The check on exit reads the
                // counters of an OOM which ended the process anyway.
                biased;
                _ = token.cancelled() => {
                    debug!("Loop cancelled");
                    return cgroup.check_on_exit();
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
                        debug!("Got remove event, checking once the process exited");
                        break;
                    }

                    if cgroup.check(cgroup.is_outer_event(&event)) {
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
        token.cancelled().await;
        cgroup.check_on_exit()
    }

    /// Watch the memory events of the container and, if present, the ones of
    /// its wrapper cgroup.
    async fn subscribe(
        events: &MemoryEvents,
        parent_events: Option<&MemoryEvents>,
        pod_events: Option<&Path>,
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
            match watcher.subscribe(&parent.path, events_tx.clone()).await {
                Ok(subscription) => parent_subscription = Some(subscription),
                Err(e) => debug!("Could not watch parent memory events: {:#}", e),
            }
        }

        // The kills of siblings advance the baseline of the outer cgroup.
        let mut pod_subscription = None;
        if let Some(pod) = pod_events {
            debug!("Also watching outer memory events: {}", pod.display());
            match watcher.subscribe(pod, events_tx).await {
                Ok(subscription) => pod_subscription = Some(subscription),
                Err(e) => debug!("Could not watch pod memory events: {:#}", e),
            }
        }

        Ok(Subscriptions {
            rx,
            _subscription: subscription,
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

    /// Checks if a file does not exist on disk.
    fn file_not_found(f: impl AsRef<Path>) -> bool {
        std::fs::metadata(f).is_err_and(|e| e.kind() == ErrorKind::NotFound)
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

    /// Create the OOM files. Failures are logged, the OOM gets reported
    /// through the channel either way.
    pub async fn write_oom_files(exit_paths: &[PathBuf]) {
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
            for line in BufReader::new(fp).lines() {
                let line = line.context("read line from cgroup file")?;
                if let Some(path) = Self::cgroup_v1_path(&line, subsystem)? {
                    return Ok(path.into());
                }
            }

            bail!("no path found")
        }

        Ok(None)
    }

    fn process_cgroup_subsystem_path_cgroup_v2(pid: u32) -> Result<Option<PathBuf>> {
        if let Some(fp) = Self::try_open_cgroup_path(pid)? {
            return Self::unified_cgroup_path(BufReader::new(fp)).map(Some);
        }

        Ok(None)
    }

    /// The cgroupfs path of the unified hierarchy from a `/proc/<pid>/cgroup`
    /// file. Named cgroup v1 hierarchies, like the one of systemd, are
    /// listed as well and may come first.
    fn unified_cgroup_path(cgroup_file: impl BufRead) -> Result<PathBuf> {
        for line in cgroup_file.lines() {
            let line = line.context("read line from cgroup file")?;
            if line.starts_with("0::") {
                return Self::cgroup_v2_path(&line);
            }
        }
        bail!("no unified hierarchy found in cgroup file")
    }

    /// The cgroupfs path of `subsystem` if the `/proc/<pid>/cgroup` line
    /// `hierarchy-ID:controller-list:cgroup-path` is the one of its hierarchy.
    /// The path may contain colons itself.
    fn cgroup_v1_path(line: &str, subsystem: &str) -> Result<Option<PathBuf>> {
        let mut iter = line.splitn(3, ':').skip(1);
        let controllers = iter.next().context("no system found in cgroup")?;
        let path = Self::strip_cgroup_root(iter.next())?;

        if controllers.is_empty() || controllers.split(',').any(|c| c == subsystem) {
            return Ok(Some(Path::new(CGROUP_ROOT).join(subsystem).join(path)));
        }
        Ok(None)
    }

    /// The cgroupfs path of the unified hierarchy line `0::cgroup-path` of
    /// `/proc/<pid>/cgroup`.
    fn cgroup_v2_path(line: &str) -> Result<PathBuf> {
        let path = Self::strip_cgroup_root(line.splitn(3, ':').nth(2))?;
        Ok(Path::new(CGROUP_ROOT).join(path))
    }

    /// The cgroup path relative to the root of its hierarchy.
    fn strip_cgroup_root(path: Option<&str>) -> Result<&str> {
        Ok(path
            .context("no path found in cgroup")?
            .strip_prefix("/")
            .context("strip root path prefix")?
            .trim_end())
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
                        debug!("No OOM baseline of the outer cgroup: {:#}", e);
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

    /// Whether a watch event concerns the outer cgroup.
    fn is_outer_event(&self, event: &Event) -> bool {
        self.outer
            .as_ref()
            .is_some_and(|(path, _)| event.paths.contains(path))
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
                debug!("Unable to read outer OOM counters: {:#}", e);
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
    rx: Receiver<WatchResult>,
    _subscription: Subscription,
    _parent_subscription: Option<Subscription>,
    _pod_subscription: Option<Subscription>,
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
        // Unsubscribing happens on the blocking pool, wait until it is done.
        for _ in 0..200 {
            if lock(&watcher.subscribers)?
                .get(file.path())
                .is_some_and(|subs| subs.len() == 1)
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(lock(&watcher.subscribers)?[file.path()].len(), 1);

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

    /// Let the watch task take its baselines.
    async fn baselines_taken() {
        sleep(Duration::from_millis(10)).await;
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
        let (_token, mut task) = watch_v2(&leaf);

        // The watch may not be in place yet, keep counting up.
        for i in 1..=500 {
            write_events(&leaf, i, i)?;
            if let Ok(event) = timeout(Duration::from_millis(10), &mut task).await {
                assert_eq!(event?, OOMEvent::new(true));
                return Ok(());
            }
        }
        bail!("no OOM found")
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
        baselines_taken().await;

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

    /// Watch the leaf, remove the scope, let `after_removal` count kills and
    /// return what the watch found on exit.
    async fn removed_scope(
        crun: bool,
        before_removal: impl Fn(&Path) -> Result<()>,
        after_removal: impl Fn(&Path, &Path) -> Result<()>,
    ) -> Result<OOMEvent> {
        let root = tempfile::tempdir()?;
        let [qos, pod, scope, leaf] = fake_pod(root.path(), crun)?;
        let (token, task) = watch_v2(&leaf);
        baselines_taken().await;

        before_removal(&pod)?;
        std::fs::remove_dir_all(&scope)?;
        after_removal(&qos, &pod)?;
        sleep(Duration::from_millis(50)).await;
        // Waiting for the exit, not taken for an OOM on its own.
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
            let event = removed_scope(crun, |_| Ok(()), |_, pod| write_events(pod, 0, 4)).await?;
            assert_eq!(event, possible);
            assert!(event.is_oom(137));
            assert!(!event.is_oom(0));

            // Without one, it is no OOM.
            let event = removed_scope(crun, |_| Ok(()), |_, _| Ok(())).await?;
            assert_eq!(event, OOMEvent::default());
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cgroup_v2_sibling_kills_do_not_count() -> Result<()> {
        for crun in [false, true] {
            // A sibling got killed while the leaf existed, the watch notices
            // it once its watches are in place.
            let sibling_kill = |pod: &Path| {
                for _ in 0..50 {
                    write_events(pod, 0, 4)?;
                    std::thread::sleep(Duration::from_millis(10));
                }
                // The leaf goes away only after the events got handled.
                std::thread::sleep(Duration::from_millis(100));
                Ok(())
            };
            let event = removed_scope(crun, sibling_kill, |_, _| Ok(())).await?;
            assert_eq!(event, OOMEvent::default());
        }
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v2_is_not_compared_above_the_pod() -> Result<()> {
        // The QoS cgroup has other pods.
        let event = removed_scope(false, |_| Ok(()), |qos, _| write_events(qos, 0, 4)).await?;
        assert_eq!(event, OOMEvent::default());

        // Without the pod cgroup, nothing tells.
        let event = removed_scope(
            false,
            |_| Ok(()),
            |_, pod| Ok(std::fs::remove_dir_all(pod)?),
        )
        .await?;
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
            baselines_taken().await;

            if sibling_kill {
                for _ in 0..50 {
                    write_events(&parent, 0, 4)?;
                    sleep(Duration::from_millis(10)).await;
                }
                sleep(Duration::from_millis(100)).await;
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
    fn oom_event_needs_kill_for_ancestor_oom() {
        assert!(OOMEvent::new(true).is_oom(0));
        assert!(!OOMEvent::default().is_oom(137));
        let ancestor = OOMEvent {
            oom: false,
            possible_oom: true,
        };
        assert!(ancestor.is_oom(137));
        assert!(!ancestor.is_oom(143));
        assert!(!ancestor.is_oom(1));
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

    #[test]
    fn cgroup_paths_from_proc_lines() -> Result<()> {
        let root = Path::new(CGROUP_ROOT);
        assert_eq!(
            OOMWatcher::cgroup_v1_path("4:memory:/kubepods/pod/ctr\n", "memory")?,
            Some(root.join("memory/kubepods/pod/ctr"))
        );
        assert_eq!(
            OOMWatcher::cgroup_v1_path("3:cpuset,memory:/a:b", "memory")?,
            Some(root.join("memory/a:b"))
        );
        // A controller only matches as a whole.
        assert_eq!(
            OOMWatcher::cgroup_v1_path("7:memory_extra:/x", "memory")?,
            None
        );
        assert_eq!(
            OOMWatcher::cgroup_v1_path("5:cpu,cpuacct:/x", "memory")?,
            None
        );
        assert!(OOMWatcher::cgroup_v1_path("5:memory", "memory").is_err());
        assert!(OOMWatcher::cgroup_v1_path("5:memory:relative", "memory").is_err());

        assert_eq!(
            OOMWatcher::cgroup_v2_path("0::/kubepods.slice/crio-1:2.scope\n")?,
            root.join("kubepods.slice/crio-1:2.scope")
        );
        assert_eq!(OOMWatcher::cgroup_v2_path("0::/")?, root.to_path_buf());
        assert!(OOMWatcher::cgroup_v2_path("0:").is_err());

        // A named v1 hierarchy may come before the unified one.
        assert_eq!(
            OOMWatcher::unified_cgroup_path(
                &b"1:name=systemd:/user.slice\n0::/kubepods.slice/ctr\n"[..]
            )?,
            root.join("kubepods.slice/ctr")
        );
        assert_eq!(
            OOMWatcher::unified_cgroup_path(&b"0::/ctr\n"[..])?,
            root.join("ctr")
        );
        assert!(OOMWatcher::unified_cgroup_path(&b"1:name=systemd:/x\n"[..]).is_err());
        Ok(())
    }

    #[test]
    fn parse_oom_control() {
        assert_eq!(
            OomControl::parse("oom_kill_disable 0\nunder_oom 0\noom_kill 3\n"),
            OomControl {
                kill_disable: false,
                under_oom: false,
                oom_kill: Some(3),
            }
        );
        // Kernels before 4.13 have no counter.
        assert_eq!(
            OomControl::parse("oom_kill_disable 1\nunder_oom 1\n"),
            OomControl {
                kill_disable: true,
                under_oom: true,
                oom_kill: None,
            }
        );
    }

    /// A fake memory cgroup v1 with its `memory.oom_control` and a pipe which
    /// stands in for the eventfd.
    struct FakeCgroupV1 {
        dir: tempfile::TempDir,
        event_tx: tokio::io::DuplexStream,
        event_rx: Option<tokio::io::DuplexStream>,
        token: CancellationToken,
    }

    impl FakeCgroupV1 {
        fn new(oom_control: &str) -> Result<Self> {
            let dir = tempfile::tempdir()?;
            std::fs::write(dir.path().join("memory.oom_control"), oom_control)?;
            let (event_tx, event_rx) = tokio::io::duplex(64);
            Ok(Self {
                dir,
                event_tx,
                event_rx: Some(event_rx),
                token: CancellationToken::new(),
            })
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join("memory.oom_control")
        }

        fn set(&self, oom_control: &str) -> Result<()> {
            Ok(std::fs::write(self.path(), oom_control)?)
        }

        /// Signal the eventfd `count` times since the last read.
        async fn signal(&mut self, count: u64) -> Result<()> {
            Ok(self.event_tx.write_all(&count.to_ne_bytes()).await?)
        }

        fn watch(&mut self, baseline: Option<u64>) -> task::JoinHandle<bool> {
            let watch = self.watch_event(baseline);
            task::spawn(async move { watch.await.is_ok_and(|event| event.oom) })
        }

        fn watch_event(&mut self, baseline: Option<u64>) -> task::JoinHandle<OOMEvent> {
            let event_rx = self.event_rx.take().expect("watched once");
            let token = self.token.clone();
            let path = self.path();
            task::spawn(async move {
                OOMWatcher::watch_cgroup_v1(&token, event_rx, &path, baseline).await
            })
        }
    }

    const KILLS_0: &str = "oom_kill_disable 0\nunder_oom 0\noom_kill 0\n";
    const KILLS_1: &str = "oom_kill_disable 0\nunder_oom 0\noom_kill 1\n";

    #[tokio::test]
    async fn cgroup_v1_event_with_kill_is_oom() -> Result<()> {
        let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
        let watch = cgroup.watch(Some(0));
        cgroup.set(KILLS_1)?;
        cgroup.signal(1).await?;
        assert!(timeout(Duration::from_secs(5), watch).await??);
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v1_event_without_kill_is_no_oom() -> Result<()> {
        // An OOM of the pod cgroup notifies this one too, the victim was a
        // sibling.
        let mut cgroup = FakeCgroupV1::new(KILLS_1)?;
        let watch = cgroup.watch(Some(1));
        cgroup.signal(1).await?;
        cgroup.token.cancel();
        assert!(!timeout(Duration::from_secs(5), watch).await??);
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v1_kill_found_on_exit() -> Result<()> {
        // Whether the event got handled before or not, the kill counts.
        let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
        let watch = cgroup.watch(Some(0));
        cgroup.signal(1).await?;
        cgroup.set(KILLS_1)?;
        cgroup.token.cancel();
        assert!(timeout(Duration::from_secs(5), watch).await??);
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v1_removal_is_no_oom() -> Result<()> {
        let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
        let watch = cgroup.watch(Some(0));
        std::fs::remove_file(cgroup.path())?;
        cgroup.signal(1).await?;
        // No exit needed, the watch ends with the cgroup.
        assert!(!timeout(Duration::from_secs(5), watch).await??);

        // A notification which came before the removal may be one.
        let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
        let watch = cgroup.watch_event(Some(0));
        std::fs::remove_file(cgroup.path())?;
        cgroup.signal(2).await?;
        let event = timeout(Duration::from_secs(5), watch).await??;
        assert_eq!(event, OOMEvent::possible(true));
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v1_event_without_counter_is_oom() -> Result<()> {
        let mut cgroup = FakeCgroupV1::new("oom_kill_disable 0\nunder_oom 1\n")?;
        let watch = cgroup.watch(None);
        cgroup.signal(1).await?;
        assert!(timeout(Duration::from_secs(5), watch).await??);
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v1_broken_eventfd_checks_on_exit() -> Result<()> {
        let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
        let watch = cgroup.watch(Some(0));
        // A short read is no event.
        cgroup.event_tx.write_all(&[1]).await?;
        cgroup.event_tx.shutdown().await?;
        cgroup.set(KILLS_1)?;
        cgroup.token.cancel();
        assert!(timeout(Duration::from_secs(5), watch).await??);
        Ok(())
    }

    /// Longer than all reads which confirm an event.
    const CONFIRM_TIME: Duration = Duration::from_secs(2);

    #[tokio::test(start_paused = true)]
    async fn cgroup_v1_kill_counted_after_event() -> Result<()> {
        // The kernel signals before it kills, the counter follows later.
        let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
        let watch = cgroup.watch(Some(0));
        cgroup.signal(1).await?;
        sleep(Duration::from_millis(30)).await;
        cgroup.set(KILLS_1)?;
        assert!(timeout(Duration::from_secs(5), watch).await??);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cgroup_v1_removed_while_confirming_needs_a_kill() -> Result<()> {
        // systemd removes the scope before the kill showed up. That may also
        // be an OOM of a sibling under the limit of the pod and a normal
        // exit, only the exit status tells.
        let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
        let watch = cgroup.watch_event(Some(0));
        cgroup.signal(1).await?;
        sleep(Duration::from_millis(30)).await;
        std::fs::remove_file(cgroup.path())?;
        let event = timeout(Duration::from_secs(5), watch).await??;
        assert_eq!(event, OOMEvent::possible(true));
        assert!(event.is_oom(128 + Signal::SIGKILL as i32));
        assert!(!event.is_oom(0));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cgroup_v1_removal_after_unconfirmed_event_is_no_oom() -> Result<()> {
        // A sibling got killed and this container survived it, a later
        // normal stop removes the cgroup.
        let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
        let watch = cgroup.watch(Some(0));
        cgroup.signal(1).await?;
        sleep(CONFIRM_TIME).await;
        assert!(!watch.is_finished());

        // Gone before the exit check.
        std::fs::remove_file(cgroup.path())?;
        cgroup.token.cancel();
        assert!(!timeout(Duration::from_secs(5), watch).await??);

        // Also if the removal signals the eventfd first.
        let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
        let watch = cgroup.watch(Some(0));
        cgroup.signal(1).await?;
        sleep(CONFIRM_TIME).await;
        std::fs::remove_file(cgroup.path())?;
        cgroup.signal(1).await?;
        assert!(!timeout(Duration::from_secs(5), watch).await??);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cgroup_v1_unconfirmed_event_of_existing_cgroup_is_no_oom() -> Result<()> {
        // A sibling got killed, the cgroup stays and is not under OOM.
        let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
        let watch = cgroup.watch(Some(0));
        cgroup.signal(1).await?;
        sleep(CONFIRM_TIME).await;
        cgroup.token.cancel();
        assert!(!timeout(Duration::from_secs(5), watch).await??);
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v1_unknown_baseline_is_no_oom_on_exit() -> Result<()> {
        // Kills from before the container, without an event since.
        let mut cgroup = FakeCgroupV1::new("oom_kill_disable 0\nunder_oom 0\noom_kill 5\n")?;
        let watch = cgroup.watch(None);
        cgroup.token.cancel();
        assert!(!timeout(Duration::from_secs(5), watch).await??);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cgroup_v1_under_oom_is_oom() -> Result<()> {
        // The victim is in a nested memcg, which the counter misses.
        let mut cgroup = FakeCgroupV1::new("oom_kill_disable 0\nunder_oom 1\noom_kill 0\n")?;
        let watch = cgroup.watch(Some(0));
        cgroup.signal(1).await?;
        assert!(timeout(Duration::from_secs(5), watch).await??);

        // Without the OOM killer the tasks wait in the OOM, which counts
        // right away.
        let mut cgroup = FakeCgroupV1::new("oom_kill_disable 1\nunder_oom 1\noom_kill 0\n")?;
        let start = time::Instant::now();
        let watch = cgroup.watch(Some(0));
        cgroup.signal(1).await?;
        assert!(timeout(Duration::from_secs(5), watch).await??);
        assert!(start.elapsed() < OOMWatcher::CONFIRM_BACKOFF[0]);
        Ok(())
    }

    #[tokio::test]
    async fn cgroup_v1_unknown_baseline_takes_event_as_oom() -> Result<()> {
        let mut cgroup = FakeCgroupV1::new(KILLS_1)?;
        let watch = cgroup.watch(None);
        cgroup.signal(1).await?;
        assert!(timeout(Duration::from_secs(5), watch).await??);
        Ok(())
    }
}
