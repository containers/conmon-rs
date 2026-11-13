//! Child process reaping and management.
use crate::{
    attach::StopOutput,
    container_io::{ContainerIO, SharedContainerIO},
    mutex::lock,
    process::{Process, pidfd_open},
};
use anyhow::{Context, Result, bail};
use nix::{errno::Errno, sys::signal::Signal, unistd::Pid};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    pin::pin,
    str,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{Notify, broadcast::Receiver, watch},
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, info};

mod orphan_sweep;
mod runtime_call;
mod watcher;

pub(crate) use runtime_call::{RuntimeCall, StartedChild};

/// What kind of process a child is. Execs are registered under the id of
/// their container, but only the container itself is the target of
/// container level operations like attach or log reopening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChildKind {
    Container,
    Exec,
}

/// A container or exec, as it gets registered with the child reaper.
#[derive(Debug)]
pub(crate) struct Child {
    pub(crate) id: Box<str>,
    pub(crate) kind: ChildKind,
    pub(crate) pid: Pid,
    pub(crate) exit_paths: Vec<PathBuf>,
    pub(crate) oom_exit_paths: Vec<PathBuf>,
    pub(crate) timeout: Option<Instant>,
    pub(crate) io: SharedContainerIO,
    pub(crate) token: CancellationToken,

    /// The process, if the caller holds it already, like an exec, so that
    /// the registration does not open a second pidfd for it. The
    /// registration opens it otherwise.
    pub(crate) process: Option<Arc<Process>>,
}

/// Watches the containers and execs of the server until they exit and
/// reaps them.
#[derive(Debug, Default)]
pub(crate) struct ChildReaper {
    /// The registered children, each until its watcher ended.
    grandchildren: Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>>,

    /// Notified whenever a watcher ended and forgot its child, which the
    /// shutdown waits for.
    forgotten: Arc<Notify>,

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

    /// Cancelled by the shutdown once the runtime calls in progress did not
    /// end within [`ChildReaper::IN_FLIGHT_WAIT`]. A call whose runtime
    /// still runs fails then and kills it on its own, which only the call
    /// can do safely, see [`ChildReaper::kill_pending`].
    stop_calls: CancellationToken,
}

/// Counts a runtime call as in progress until dropped. A successful
/// [`ChildReaper::create_child`] hands it to the caller, which passes it to
/// [`ChildReaper::watch_grandchild`], so the call ends with the registration
/// of its child, or wherever the caller gives up.
#[derive(Debug)]
#[cfg_attr(test, derive(Default))]
pub(crate) struct InFlight {
    calls: Option<Arc<watch::Sender<Calls>>>,

    /// The number of the call, see [`Calls`].
    seq: u64,

    /// Keeps the process which the call started pending until the call
    /// ends, also if the call gets dropped before its registration, see
    /// [`InFlight::claim`].
    pending: Option<PendingPid>,

    /// Cancelled once the shutdown stops the call, see
    /// [`ChildReaper::stop_calls`].
    stop: CancellationToken,
}

impl InFlight {
    /// Mark the process `pid` which the call started as pending until the
    /// call ends, and record it for the shutdown, which kills it if the
    /// call does not end in time, see [`ChildReaper::kill_pending`]. It is
    /// pending before it gets recorded. A shutdown which did not see the
    /// record stopped the call before it looked at the calls, so the call
    /// sees that here and kills the process itself.
    fn claim(&mut self, pending: &PendingPids, pid: Pid) {
        self.pending = Some(PendingPid::new(pending, pid));
        if let Some(calls) = &self.calls {
            calls.send_if_modified(|calls| {
                if let Some(call) = calls.in_progress.get_mut(&self.seq) {
                    call.process = Some(pid);
                }
                // Nobody waits for it.
                false
            });
        }
        if self.stop.is_cancelled() {
            debug!(%pid, "Killing process of a runtime call stopped by the shutdown");
            kill_grandchild(pid, Signal::SIGKILL);
        }
    }
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

    /// The calls in progress by their number.
    in_progress: BTreeMap<u64, Call>,
}

/// A runtime call in progress, see [`Calls`].
#[derive(Debug)]
struct Call {
    /// Until when the call may still claim a process: the deadline of its
    /// runtime and the [`ChildReaper::CLAIM_MARGIN`].
    claim_by: Instant,

    /// The process which the call started, once it claimed it, see
    /// [`InFlight::claim`].
    process: Option<Pid>,
}

impl Calls {
    /// Whether a call numbered below `before` is in progress, which may
    /// still claim a process at `now`. The deadlines of the calls differ, an
    /// older one may still claim one while a younger one cannot anymore.
    fn may_claim_before(&self, before: u64, now: Instant) -> bool {
        self.in_progress
            .range(..before)
            .any(|(_, call)| now < call.claim_by)
    }

    /// Whether no call is in progress.
    fn ended(&self) -> bool {
        self.in_progress.is_empty()
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
/// counted: a call keeps the mark of its runtime until it reaped it, and the
/// PID of a runtime which got reaped may already be in use by the process of
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
    pub(crate) fn get(&self, id: &str) -> Result<RegisteredContainer> {
        lock(&self.grandchildren)
            .get(id)
            .and_then(|v| v.iter().find_map(ReapableChild::container))
            .with_context(|| format!("{CONTAINER_NOT_FOUND}: {id}"))
    }

    /// Start a runtime call whose runtime gets killed at the `deadline`,
    /// unless the server shuts down. The shutdown waits for the calls in
    /// progress, see [`ChildReaper::kill_grandchildren`].
    fn begin_call(&self, deadline: Instant) -> Result<InFlight> {
        // Under the lock the shutdown takes its snapshot with, so that it
        // either sees this call or this call sees the shutdown.
        let _map = lock(&self.grandchildren);
        if self.shutting_down.load(Ordering::SeqCst) {
            bail!("server is shutting down")
        }
        // A deadline too far in the future to add the margin to is never
        // reached either.
        let claim_by = deadline.checked_add(Self::CLAIM_MARGIN).unwrap_or(deadline);
        let mut seq = 0;
        self.in_flight.send_modify(|calls| {
            seq = calls.next;
            calls.next += 1;
            calls.in_progress.insert(
                seq,
                Call {
                    claim_by,
                    process: None,
                },
            );
        });
        Ok(InFlight {
            calls: Some(self.in_flight.clone()),
            seq,
            pending: None,
            stop: self.stop_calls.clone(),
        })
    }

    /// Register and watch a child which `create_child` started, which ends
    /// its call.
    pub(crate) fn watch_grandchild(
        &self,
        child: Child,
        call: InFlight,
    ) -> Result<Receiver<ExitChannelData>> {
        let pid = child.pid;
        let (id, reapable_grandchild, mut exit) = ReapableChild::from_child(child);
        let seq = reapable_grandchild.seq;

        let mut map = lock(&self.grandchildren);
        // A child registered after the shutdown took its snapshot, under this
        // lock, is in the map as well, which the shutdown waits to empty. Its
        // call fails below, so it writes no exit files.
        let shutting_down = self.shutting_down.load(Ordering::SeqCst);
        if shutting_down {
            exit.drop_exit_paths();
        }
        // Under the lock, so that the child is registered before it can be
        // forgotten. A task which never runs, like one spawned while the
        // runtime shuts down, does not forget it, see `watch`.
        let (grandchildren, forgotten) = (self.grandchildren.clone(), self.forgotten.clone());
        let key = id.clone();
        let exit_rx = reapable_grandchild.watch(exit, move || {
            Self::forget_grandchild(&grandchildren, &key, seq);
            forgotten.notify_waiters();
        });
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

    /// Register and watch an exec which `create_child` started under the
    /// `id` of its container, which gets killed at its `timeout`, see
    /// [`ChildReaper::watch_grandchild`].
    pub(crate) fn watch_exec(
        &self,
        id: Box<str>,
        started: StartedChild,
        timeout: Option<Instant>,
        io: ContainerIO,
    ) -> Result<WatchedExec> {
        let StartedChild { pid, token, call } = started;
        // Not reaped before the registration below, so the PID is still its
        // own. Shared with the registration, one pidfd per exec.
        let process = Arc::new(Process::open(pid));
        let io = SharedContainerIO::new(io);
        let child = Child {
            id,
            kind: ChildKind::Exec,
            pid,
            exit_paths: vec![],
            oom_exit_paths: vec![],
            timeout,
            io: io.clone(),
            token: token.clone(),
            process: Some(process.clone()),
        };
        // The output stops once the process exited. Also if the registration
        // fails, its watcher reports the exit.
        let stop_output = io.attach().stop_output_on_exit(token.clone());
        // The registration fails only while the server shuts down, which
        // killed the process already.
        let exit_rx = self.watch_grandchild(child, call)?;
        Ok(WatchedExec {
            io,
            token,
            process,
            exit_rx,
            stop_output,
        })
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

    /// Forward SIGTERM to the containers like conmon, to their PIDs only,
    /// and wait for their watchers until the `deadline`. Containers
    /// registered meanwhile get neither, the shutdown kills them and what is
    /// left afterwards. A container stays registered after its exit while
    /// its watcher finishes the output, its PID may be in use by another
    /// child by then, so it gets signalled through its pidfd, see
    /// [`ReapableChild::process`].
    pub(crate) async fn terminate_containers(&self, deadline: Instant) {
        let containers: HashSet<_> = lock(&self.grandchildren)
            .values()
            .flatten()
            .filter(|gc| gc.is_container())
            .inspect(|gc| {
                if let Err(e) = gc.process.signal(Signal::SIGTERM) {
                    debug!(pid = %gc.pid, "Not signalling container, it is gone: {e}");
                }
            })
            .map(|gc| gc.seq)
            .collect();
        // Nothing to tell for a server without containers.
        if !containers.is_empty() {
            info!("Sent SIGTERM to {} containers", containers.len());
        }
        let signalled = |gc: &ReapableChild| containers.contains(&gc.seq);
        let left = self.wait_for_watchers(deadline, signalled).await;
        if !left.is_empty() {
            info!("Containers with PIDs {left:?} did not exit on SIGTERM");
        }
    }

    /// Kill every grandchild and wait for their watchers and the runtime
    /// calls in progress until the `deadline` of the shutdown, also for the
    /// execs killed at their timeout which were not reaped yet. A process
    /// which does not exit on SIGKILL, like one stuck in the kernel, must not
    /// keep the server from stopping. It fails if they did not end in time.
    pub(crate) async fn kill_grandchildren(&self, deadline: Instant) -> Result<()> {
        let processes: Vec<_> = {
            let map = lock(&self.grandchildren);
            // Children registered after this snapshot get killed on their
            // registration, see `watch_grandchild`.
            self.shutting_down.store(true, Ordering::SeqCst);
            map.values()
                .flatten()
                .map(|gc| gc.process.clone())
                .collect()
        };
        if !processes.is_empty() {
            info!("Killing {} containers and execs", processes.len());
        }
        // Signal every grandchild first, their watchers end concurrently, so
        // that the shutdown takes as long as the slowest child instead of the
        // sum of all of them. One which exited, whose watcher still finishes
        // the output, only gets what remains of its group signalled, which
        // may hold the pipes, see `terminate_containers`.
        for process in processes {
            let pid = process.pid();
            let span = debug_span!("kill_grandchild", %pid);
            let _enter = span.enter();
            debug!("Killing single grandchild");
            if let Err(e) = process.signal_group(Signal::SIGKILL) {
                debug!("Not signalling grandchild, it is gone: {e}");
            }
        }
        // An init of a PID namespace only exits once every process of the
        // namespace is reaped, including zombies reparented to this process.
        // The sweep keeps running while the watchers are awaited.
        self.sweep.notify_one();

        // Runtime calls in progress get to finish, the server would kill a
        // runtime halfway otherwise. Their children got killed on
        // registration. A call which hangs does not take the time of the
        // watchers, which end meanwhile. It gets stopped early enough to
        // clean up before the deadline.
        let mut in_flight = self.in_flight.subscribe();
        let stop_at = deadline
            .checked_sub(Self::STOP_BUDGET)
            .unwrap_or(deadline)
            .min(Instant::now() + Self::IN_FLIGHT_WAIT);
        let ended_in_time = time::timeout_at(stop_at, in_flight.wait_for(Calls::ended))
            .await
            .is_ok();
        if !ended_in_time {
            self.stop_calls.cancel();
            self.kill_pending();
        }
        // Also the watchers of the children which the calls registered, and
        // the calls which got stopped, which still clean up.
        let (left, calls_ended) = tokio::join!(self.wait_for_watchers(deadline, |_| true), async {
            ended_in_time
                || time::timeout_at(deadline, in_flight.wait_for(Calls::ended))
                    .await
                    .is_ok()
        },);

        let calls = match (ended_in_time, calls_ended) {
            (true, _) => None,
            (false, true) => Some("runtime calls in progress got stopped"),
            (false, false) => Some("runtime calls still in progress"),
        };
        match (left.is_empty(), calls) {
            (true, None) => {
                debug!("Done killing all grandchildren");
                Ok(())
            }
            (true, Some(calls)) => bail!("{calls}"),
            (false, None) => bail!("PIDs {left:?} did not exit"),
            (false, Some(calls)) => bail!("PIDs {left:?} did not exit, {calls}"),
        }
    }

    /// How long the shutdown waits for runtime calls in progress to end on
    /// their own, within its deadline. Those which take longer get stopped,
    /// see [`ChildReaper::kill_pending`], and get the rest of the deadline
    /// to end, at least the [`ChildReaper::STOP_BUDGET`].
    const IN_FLIGHT_WAIT: Duration = Duration::from_secs(5);

    /// How long before the deadline of the shutdown the runtime calls in
    /// progress get stopped at the latest. A stopped call kills its runtime
    /// right away, waits for its exit and its output only briefly, and kills
    /// the process the runtime left, see `stop_runtime`, which this covers
    /// with some time to spare for the scheduling.
    pub(crate) const STOP_BUDGET: Duration = Duration::from_secs(1);

    /// Kill the processes which the runtime calls that did not end in time
    /// started but did not register yet, like a created container whose
    /// call waits for the output of its runtime. The server stops without
    /// the calls, a process they leave would run without a watcher.
    ///
    /// The calls got stopped before, see [`ChildReaper::stop_calls`]. Their
    /// runtimes are not signalled here: tokio reaps them without the
    /// `REAP_LOCK` of [`Process`], so the PID of one may belong to another
    /// process by now. A call kills its runtime itself while it still holds
    /// it unreaped, see `stop_runtime`. The processes which the calls
    /// claimed get reaped only by the orphan sweep and by their watchers,
    /// under that lock, see [`kill_grandchild`]. Under the lock of the
    /// grandchildren no call registers its process meanwhile, whose watcher
    /// could reap it then, and under the one of the calls no call ends,
    /// after which the sweep could reap it. A call which claims its process
    /// later kills it itself, see [`InFlight::claim`].
    fn kill_pending(&self) {
        let _map = lock(&self.grandchildren);
        let calls = self.in_flight.borrow();
        for pid in calls.in_progress.values().filter_map(|call| call.process) {
            debug!(%pid, "Killing process of a runtime call in progress");
            kill_grandchild(pid, Signal::SIGKILL);
        }
    }

    /// Wait until the watchers of all grandchildren which match the `filter`
    /// ended, every watcher forgets its child at its end. Return the PIDs of
    /// the children left at the deadline.
    async fn wait_for_watchers(
        &self,
        deadline: Instant,
        filter: impl Fn(&ReapableChild) -> bool,
    ) -> Vec<i32> {
        loop {
            // Enabled before the check, so that no end gets missed.
            let mut forgotten = pin!(self.forgotten.notified());
            forgotten.as_mut().enable();
            let mut left: Vec<_> = lock(&self.grandchildren)
                .values()
                .flatten()
                .filter(|gc| filter(gc))
                .map(|gc| gc.pid.as_raw())
                .collect();
            if left.is_empty() || Instant::now() >= deadline {
                left.sort_unstable();
                return left;
            }
            debug!("Waiting for {} watchers to end", left.len());
            // Checked again at the deadline.
            let _ = time::timeout_at(deadline, forgotten).await;
        }
    }
}

/// Signal a grandchild and its process group, unless it got reaped already,
/// see [`Process::signal_group`]. The grandchildren got reparented to this
/// subreaper, so the PID refers to a child.
fn kill_grandchild(pid: Pid, s: Signal) {
    if let Err(e) = Process::child(pid).signal_group(s) {
        debug!(%pid, "Not signalling grandchild, it is gone: {e}");
    }
}

/// An exec which got registered, see [`ChildReaper::watch_exec`].
pub(crate) struct WatchedExec {
    pub(crate) io: SharedContainerIO,

    /// Cancelled once the process exited.
    pub(crate) token: CancellationToken,

    /// The process, which its watcher may reap before the token got
    /// cancelled, so it does not get signalled by its PID.
    pub(crate) process: Arc<Process>,
    pub(crate) exit_rx: Receiver<ExitChannelData>,

    /// Stops the output once dropped by its consumer after the exit.
    pub(crate) stop_output: StopOutput,
}

/// A registered child, until its watcher ended. What only the watcher needs
/// moves into it, see [`ExitHandling`](watcher::ExitHandling).
#[derive(Clone, Debug)]
pub(crate) struct ReapableChild {
    /// Tells the registrations apart, unlike the PID, which gets reused.
    seq: u64,
    pid: Pid,

    /// Opened at the registration, before the watcher could reap the child.
    /// The watcher reaps it before it cancels the token, or after that for
    /// one killed at its timeout. Once it got reaped, the PID may belong to
    /// another process, which the signals through the pidfd do not reach.
    /// Kernels without pidfds (before 5.3) only get the check of the PID,
    /// see [`Process::open`].
    process: Arc<Process>,

    /// Set once the watcher reaped the child. It stays registered while the
    /// watcher finishes its output, its PID may belong to another child by
    /// then, which the registration must not claim, see
    /// [`ChildReaper::is_claimed`].
    reaped: Arc<AtomicBool>,

    /// The stdio of a container, which container level operations use, see
    /// [`ChildReaper::get`]. None for an exec, only its consumer holds its
    /// stdio, which also tells the kinds apart. One killed at its timeout stays registered until it got
    /// reaped, which may take until the server stops, but its pipes or its
    /// terminal get released with the consumer.
    io: Option<SharedContainerIO>,
    token: CancellationToken,
}

impl ReapableChild {
    /// The PID of the child.
    pub(crate) fn pid(&self) -> Pid {
        self.pid
    }

    /// The PID of the child while its watcher still waits for it, see
    /// [`Self::reaped`].
    fn claimed_pid(&self) -> Option<Pid> {
        (!self.reaped.load(Ordering::Acquire)).then_some(self.pid)
    }

    /// Whether the child is a container, not an exec, see [`Self::io`].
    fn is_container(&self) -> bool {
        self.io.is_some()
    }

    /// The container, none for an exec.
    pub(crate) fn container(&self) -> Option<RegisteredContainer> {
        Some(RegisteredContainer {
            io: self.io.clone()?,
            token: self.token.clone(),
        })
    }
}

/// A registered container, see [`ChildReaper::get`].
#[derive(Clone, Debug)]
pub(crate) struct RegisteredContainer {
    io: SharedContainerIO,
    token: CancellationToken,
}

impl RegisteredContainer {
    /// The stdio of the container.
    pub(crate) fn io(&self) -> &SharedContainerIO {
        &self.io
    }

    /// Cancelled once the container exited.
    pub(crate) fn token(&self) -> &CancellationToken {
        &self.token
    }
}

// A stopped call ends within its budget.
const _: () = assert!(
    ChildReaper::STOP_BUDGET.as_nanos()
        > ChildReaper::STOPPED_EXIT_WAIT.as_nanos() + ChildReaper::STOPPED_OUTPUT_WAIT.as_nanos()
);

/// The start of the error of a request for a container the server does not
/// know, clients match it, see `ErrContainerNotFound` of the Go client. The
/// message of older servers is "child not available".
const CONTAINER_NOT_FOUND: &str = "container not found";

/// The exit code of an exec which could not be started, for example because
/// the runtime failed.
pub(crate) const EXIT_CODE_NOT_STARTED: i32 = -2;

/// The exit code of a process which did not exit before its timeout. Clients
/// take -3 for a timeout, no other failure may report it, except for
/// [`EXIT_CODE_WAIT_FAILED`](crate::process::EXIT_CODE_WAIT_FAILED), which
/// always did.
pub(crate) const EXIT_CODE_TIMED_OUT: i32 = -3;

/// The exit of a child, which its waiters get.
#[derive(Clone, Debug)]
pub(crate) struct ExitChannelData {
    pub(crate) exit_code: i32,
    pub(crate) timed_out: bool,
}

#[cfg(test)]
mod tests;
