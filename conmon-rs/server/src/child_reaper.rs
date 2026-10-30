//! Child process reaping and management.
use crate::{
    child::{Child, ChildKind},
    container_io::SharedContainerIO,
    mutex::lock,
    process::{Process, pidfd_open},
};
use anyhow::{Context, Result, bail};
use futures::future::join_all;
use nix::{errno::Errno, sys::signal::Signal, unistd::Pid};
use std::{
    collections::{BTreeMap, HashMap},
    os::fd::OwnedFd,
    path::PathBuf,
    str,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{Notify, broadcast::Receiver, watch},
    task::JoinHandle,
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error};

mod orphan_sweep;
mod runtime_call;
mod watcher;

pub use runtime_call::RuntimeCall;
pub(crate) use runtime_call::{RuntimeOutput, with_stderr};

#[derive(Debug, Default)]
pub struct ChildReaper {
    grandchildren: Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>>,

    /// The runtimes and container processes of calls in progress, which are
    /// not registered as grandchildren yet. Their exit status belongs to the
    /// call, so the orphan sweep leaves them alone. See [`PendingPid`].
    pending: PendingPids,

    /// Wakes the orphan sweep up early, for example after a failed runtime
    /// call or on shutdown.
    sweep: Notify,

    /// Set by the shutdown while it holds the lock of the grandchildren, so a
    /// child registered later knows that the shutdown missed it.
    shutting_down: AtomicBool,

    /// The runtime calls in progress, including the registration of their
    /// child, which the shutdown waits for.
    in_flight: Arc<watch::Sender<Calls>>,

    /// The watchers of children registered after the shutdown took its
    /// snapshot, which it waits for as well.
    late_watchers: Mutex<Vec<(Pid, TaskHandle)>>,
}

/// Counts a runtime call as in progress until dropped. A successful
/// [`ChildReaper::create_child`] hands it to the caller, which passes it to
/// [`ChildReaper::watch_grandchild`], so the call ends with the registration
/// of its child, or wherever the caller gives up.
#[derive(Debug)]
#[cfg_attr(test, derive(Default))]
pub struct InFlight {
    calls: Option<Arc<watch::Sender<Calls>>>,

    /// The number of the call, see [`Calls`].
    seq: u64,

    /// Keeps the process which the call started pending until the call
    /// ends, also if the call gets dropped before its registration.
    pending: Option<PendingPid>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(calls) = &self.calls {
            calls.send_modify(|calls| {
                calls.in_progress.remove(&self.seq);
            });
        }
    }
}

/// The runtime calls in progress, numbered in the order they started.
#[derive(Debug, Default)]
struct Calls {
    /// The number of the next call.
    next: u64,

    /// Until when each call in progress may still claim a process, by its
    /// number: the deadline of its runtime and the
    /// [`ChildReaper::CLAIM_MARGIN`]. None for a call without a deadline.
    in_progress: BTreeMap<u64, Option<Instant>>,
}

impl Calls {
    /// Whether a call numbered below `before` is in progress, which may
    /// still claim a process at `now`. The deadlines of the calls differ, an
    /// older one may still claim one while a younger one cannot anymore.
    fn may_claim_before(&self, before: u64, now: Instant) -> bool {
        self.in_progress
            .range(..before)
            .any(|(_, claim_by)| claim_by.is_none_or(|claim_by| now < claim_by))
    }
}

/// A zombie which nobody claimed, see [`ChildReaper::sweep_orphans`].
#[derive(Clone, Copy, Debug)]
struct Candidate {
    first_seen: Instant,

    /// The calls numbered below this started before it was first seen. It
    /// may be the process of one of them, which may still claim it.
    calls_before: u64,
}

/// The pending PIDs with the number of their marks, see [`PendingPid`].
type PendingPids = Arc<Mutex<HashMap<Pid, usize>>>;

/// Marks a PID as pending for the lifetime of the guard. The marks get
/// counted: a call keeps the mark of its runtime until it ends, and the PID
/// of a runtime which got reaped may already be in use by the process of
/// another call, whose mark must stay.
#[derive(Debug)]
struct PendingPid {
    pending: PendingPids,
    pid: Pid,
}

impl PendingPid {
    fn new(pending: &PendingPids, pid: Pid) -> Self {
        *lock(pending).entry(pid).or_default() += 1;
        Self {
            pending: pending.clone(),
            pid,
        }
    }
}

impl Drop for PendingPid {
    fn drop(&mut self) {
        let mut pending = lock(&self.pending);
        if let Some(marks) = pending.get_mut(&self.pid) {
            *marks -= 1;
            if *marks == 0 {
                pending.remove(&self.pid);
            }
        }
    }
}

impl ChildReaper {
    /// Retrieve the container registered under `id`. Execs share the id of
    /// their container and are skipped. The error starts with
    /// [`CONTAINER_NOT_FOUND`] if there is none.
    pub fn get(&self, id: &str) -> Result<ReapableChild> {
        let lock = lock(&self.grandchildren);
        let r = lock
            .get(id)
            .and_then(|v| v.iter().find(|c| c.kind == ChildKind::Container))
            .with_context(|| format!("{CONTAINER_NOT_FOUND}: {id}"))?
            .clone();
        Ok(r)
    }

    /// Start a runtime call whose runtime gets killed at the `deadline`,
    /// unless the server shuts down. The shutdown waits for the calls in
    /// progress, see [`ChildReaper::kill_grandchildren`].
    fn begin_call(&self, deadline: Option<Instant>) -> Result<InFlight> {
        // Under the lock the shutdown takes its snapshot with, so that it
        // either sees this call or this call sees the shutdown.
        let _map = lock(&self.grandchildren);
        if self.shutting_down.load(Ordering::SeqCst) {
            bail!("server is shutting down")
        }
        let claim_by = deadline.and_then(|deadline| deadline.checked_add(Self::CLAIM_MARGIN));
        let mut seq = 0;
        self.in_flight.send_modify(|calls| {
            seq = calls.next;
            calls.next += 1;
            calls.in_progress.insert(seq, claim_by);
        });
        Ok(InFlight {
            calls: Some(self.in_flight.clone()),
            seq,
            pending: None,
        })
    }

    /// Register and watch a child which `create_child` started, which ends
    /// its call.
    pub fn watch_grandchild(
        &self,
        child: Child,
        leak_fds: Vec<OwnedFd>,
        call: InFlight,
    ) -> Result<Receiver<ExitChannelData>> {
        let pid = child.pid;
        let (id, mut reapable_grandchild) = ReapableChild::from_child(child);
        let seq = reapable_grandchild.seq;

        let mut map = lock(&self.grandchildren);
        // Under the lock, so that the child is registered before it can be
        // forgotten. A task which never runs, like one spawned while the
        // runtime shuts down, does not forget it, see `watch`.
        let grandchildren = self.grandchildren.clone();
        let key = id.clone();
        let exit_rx = reapable_grandchild.watch(move || {
            drop(leak_fds);
            Self::forget_grandchild(&grandchildren, &key, seq);
        });
        let shutting_down = self.shutting_down.load(Ordering::SeqCst);
        if shutting_down && let Some(task) = &reapable_grandchild.task {
            lock(&self.late_watchers).push((pid, task.clone()));
        }
        map.entry(id).or_default().push(reapable_grandchild);
        // Ends the pending mark only once registered, so it is claimed all
        // the time, see `is_claimed`.
        drop(call);
        drop(map);

        // A call which was in progress while the shutdown killed the others.
        if shutting_down {
            Self::kill_late_grandchild(pid);
            bail!("server is shutting down")
        }
        Ok(exit_rx)
    }

    /// Kill a grandchild registered after the shutdown killed the others,
    /// unless it is gone already, and return whether it was signalled. The
    /// sweep does not defer orphans while the server shuts down, so the
    /// process may have exited and got reaped before its call claimed it.
    /// Opening a pidfd fails with `ESRCH` then, and neither the PID nor the
    /// group of the process gets signalled. A PID which got reused that fast
    /// only gets signalled if it belongs to an unreaped child of the server,
    /// see [`kill_grandchild`].
    fn kill_late_grandchild(pid: Pid) -> bool {
        if matches!(pidfd_open(pid), Err(Errno::ESRCH)) {
            debug!(%pid, "Not killing grandchild, it got reaped already");
            return false;
        }
        kill_grandchild(pid, Signal::SIGKILL);
        true
    }

    /// Forget the child of the registration `seq` under `id`. Not by its PID,
    /// which may be in use by a new child already.
    fn forget_grandchild(
        locked_grandchildren: &Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>>,
        id: &str,
        seq: u64,
    ) {
        let mut map = lock(locked_grandchildren);
        let Some(children) = map.get_mut(id) else {
            return;
        };
        if let Some(pos) = children.iter().position(|v| v.seq == seq) {
            children.swap_remove(pos);
        }
        // Containers get a new ID on every restart, keeping the entries of
        // the old ones would grow the map forever.
        if children.is_empty() {
            map.remove(id);
        }
    }

    /// Kill every grandchild and wait for their watchers and the runtime
    /// calls in progress until the `deadline` of the shutdown. A process
    /// which does not exit on SIGKILL, like one stuck in the kernel, must not
    /// keep the server from stopping. It fails if they did not end in time.
    pub async fn kill_grandchildren(&self, deadline: Instant) -> Result<()> {
        debug!("Killing grandchildren");
        // Collect only PIDs and task handles to avoid cloning entire ReapableChild
        let children: Vec<_> = {
            let map = lock(&self.grandchildren);
            // Children registered after this snapshot get killed on their
            // registration, see `watch_grandchild`.
            self.shutting_down.store(true, Ordering::SeqCst);
            map.values()
                .flatten()
                .map(|gc| (gc.pid, gc.task.clone()))
                .collect()
        };
        // Signal every grandchild first and wait for all of their tasks
        // concurrently, so that the shutdown takes as long as the slowest
        // child instead of the sum of all of them.
        let mut tasks = Vec::with_capacity(children.len());
        for (pid, task) in children {
            let span = debug_span!("kill_grandchild", %pid);
            let _enter = span.enter();
            debug!("Killing single grandchild");
            kill_grandchild(pid, Signal::SIGKILL);
            tasks.extend(task.map(|task| (pid, task)));
        }
        // An init of a PID namespace only exits once every process of the
        // namespace is reaped, including zombies reparented to this process.
        // The sweep keeps running while the watchers are awaited.
        self.sweep.notify_one();
        let mut left = Self::wait_for_watchers(tasks, deadline).await?;

        // Runtime calls in progress get to finish, the server would kill a
        // runtime halfway otherwise. Their children got killed on registration.
        // A call which hangs does not take the time of the late watchers.
        let mut in_flight = self.in_flight.subscribe();
        let in_flight_deadline = deadline.min(Instant::now() + Self::IN_FLIGHT_WAIT);
        let calls_ended = time::timeout_at(
            in_flight_deadline,
            in_flight.wait_for(|calls| calls.in_progress.is_empty()),
        )
        .await
        .is_ok();
        let late = std::mem::take(&mut *lock(&self.late_watchers));
        left.extend(Self::wait_for_watchers(late, deadline).await?);

        match (left.is_empty(), calls_ended) {
            (true, true) => {
                debug!("Done killing all grandchildren");
                Ok(())
            }
            (true, false) => bail!("runtime calls still in progress"),
            (false, true) => bail!("PIDs {left:?} did not exit"),
            (false, false) => bail!("PIDs {left:?} did not exit, runtime calls still in progress"),
        }
    }

    /// How long the shutdown waits for runtime calls in progress at most,
    /// within its deadline.
    const IN_FLIGHT_WAIT: Duration = Duration::from_secs(5);

    /// Wait for the watchers of the grandchildren until the deadline, and
    /// return the PIDs of the ones which did not end.
    async fn wait_for_watchers(
        tasks: Vec<(Pid, TaskHandle)>,
        deadline: Instant,
    ) -> Result<Vec<i32>> {
        let mut handles = Vec::with_capacity(tasks.len());
        for (pid, task) in tasks {
            if let Some(handle) = lock(&task).take() {
                handles.push((pid, handle));
            }
        }
        let waits = handles.iter_mut().map(|(pid, handle)| {
            async move {
                debug!("Waiting for task to close");
                if let Err(e) = handle.await {
                    error!("Unable to close grandchild: {e:#}");
                }
                debug!("Done killing single grandchild");
            }
            .instrument(debug_span!("close", %pid))
        });
        if time::timeout_at(deadline, join_all(waits)).await.is_ok() {
            return Ok(vec![]);
        }
        Ok(handles
            .iter()
            .filter(|(_, handle)| !handle.is_finished())
            .map(|(pid, _)| pid.as_raw())
            .collect())
    }
}

/// Signal a grandchild and its process group, unless it got reaped already,
/// see [`Process::signal_group`]. The grandchildren got reparented to this
/// subreaper, so the PID refers to a child.
pub fn kill_grandchild(pid: Pid, s: Signal) {
    if let Err(e) = Process::child(pid).signal_group(s) {
        debug!(%pid, "Not signalling grandchild, it is gone: {e}");
    }
}

type TaskHandle = Arc<Mutex<Option<JoinHandle<()>>>>;

#[derive(Clone, Debug)]
pub struct ReapableChild {
    /// Tells the registrations apart, unlike the PID, which gets reused.
    seq: u64,
    kind: ChildKind,
    exit_paths: Arc<[PathBuf]>,
    oom_exit_paths: Arc<[PathBuf]>,
    pid: Pid,
    io: SharedContainerIO,
    timeout: Option<Instant>,
    token: CancellationToken,
    task: Option<TaskHandle>,
    cleanup_cmd: Arc<[String]>,
}

impl ReapableChild {
    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn io(&self) -> &SharedContainerIO {
        &self.io
    }

    pub fn timeout(&self) -> Option<Instant> {
        self.timeout
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }
}

/// The start of the error of a request for a container the server does not
/// know, clients match it, see `ErrContainerNotFound` of the Go client. The
/// message of older servers is "child not available".
const CONTAINER_NOT_FOUND: &str = "container not found";

/// The exit code of an exec which could not be started, for example because
/// the runtime failed.
pub const EXIT_CODE_NOT_STARTED: i32 = -2;

/// The exit code of a process which did not exit before its timeout. Clients
/// take -3 for a timeout, no other failure may report it, except for
/// [`EXIT_CODE_WAIT_FAILED`](crate::process::EXIT_CODE_WAIT_FAILED), which
/// always did.
pub const EXIT_CODE_TIMED_OUT: i32 = -3;

#[derive(Clone, Debug)]
pub struct ExitChannelData {
    pub exit_code: i32,
    pub timed_out: bool,
}

#[cfg(test)]
mod tests;
