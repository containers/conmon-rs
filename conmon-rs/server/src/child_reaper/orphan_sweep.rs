//! Reaping the zombies nobody claimed.

use super::{Candidate, ChildReaper};
use crate::{
    mutex::lock,
    process::{has_zombie, reap_guard, zombie_children},
};
use nix::{
    sys::wait::{WaitPidFlag, waitpid},
    unistd::Pid,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::Ordering},
};
use tokio::{
    select,
    signal::unix::{SignalKind, signal},
    task,
    time::{self, Duration, Instant},
};
use tracing::{Instrument, debug, debug_span, error};

impl ChildReaper {
    /// How long a zombie nobody claimed stays unreaped before the sweep takes
    /// it, so that an owner which is about to wait for it still can.
    const ORPHAN_GRACE: Duration = Duration::from_secs(1);

    /// How long the sweep leaves a zombie nobody claimed alone at most, for
    /// the runtime calls which were in progress when it was first seen. The
    /// process of a call can exit before the call claims it, like an exec
    /// which ends before its runtime exited, and its exit status belongs to
    /// the call. Each of these calls defers the zombie only until its own
    /// runtime deadline and the [`ChildReaper::CLAIM_MARGIN`]. This bound
    /// counts from when the zombie was first seen, the earlier of both ends
    /// the deferral. That is the one of the call for a zombie first seen
    /// late in its call, like the process of a slow exec runtime which
    /// exited right before it, and this one for a create, whose runtime gets
    /// 10 minutes: a runtime which hangs must not keep the zombies an init
    /// of a PID namespace waits for that long. An exec whose process exits
    /// more than this before its runtime does loses its exit code.
    pub(super) const ORPHAN_MAX_DEFER: Duration = Duration::from_secs(30);

    /// How long after the deadline of its runtime a call may still claim
    /// its process: a runtime which exits right before it may still take
    /// the [`ChildReaper::RUNTIME_GRACE`] to connect the terminal,
    /// the [`ChildReaper::ORPHAN_GRACE`] on top covers the scheduling.
    pub(super) const CLAIM_MARGIN: Duration =
        Self::RUNTIME_GRACE.saturating_add(Self::ORPHAN_GRACE);

    /// Reap exited children which nobody waits for, in the background.
    ///
    /// A runtime which fails after entering the PID namespace of a container,
    /// like an exec of a missing binary, leaves its helpers behind as children
    /// of this subreaper. They exit at any later time. The init of the
    /// namespace cannot exit until their zombies are reaped, so the container
    /// could never be stopped.
    ///
    /// Every other child has an owner which reaps it: the runtimes are waited
    /// for by tokio, the container processes by their watchers, and the
    /// cleanup commands and the pause init by the tasks which spawned them.
    /// They are never zombies for long, so a zombie which is neither
    /// registered nor pending for [`ChildReaper::ORPHAN_GRACE`] is an orphan,
    /// once no runtime call which may still claim it is in progress, at most
    /// for [`ChildReaper::ORPHAN_MAX_DEFER`]. Once the server shuts down,
    /// those calls cannot register a process anymore and are not waited for.
    pub fn start_orphan_sweep(self: Arc<Self>) {
        task::spawn(
            async move {
                let mut sigchld = match signal(SignalKind::child()) {
                    Ok(signal) => signal,
                    Err(e) => {
                        error!("Unable to listen for SIGCHLD, orphans stay unreaped: {e}");
                        return;
                    }
                };
                let mut candidates = HashMap::new();
                loop {
                    select! {
                        _ = sigchld.recv() => {}
                        () = self.sweep.notified() => {}
                        // Only candidates need a recheck once their grace is over.
                        () = time::sleep(Self::ORPHAN_GRACE), if !candidates.is_empty() => {}
                    }
                    self.sweep_orphans(&mut candidates, Self::ORPHAN_GRACE, Self::ORPHAN_MAX_DEFER)
                        .await;
                }
            }
            .instrument(debug_span!("orphan_sweep")),
        );
    }

    /// Reap the zombie children which have been unclaimed for `grace`, and
    /// up to `max_defer` while a call which was in progress when the zombie
    /// was first seen still is and may still claim it, unless the server
    /// shuts down. `candidates` remembers when a zombie was first seen and
    /// the calls which were started then.
    pub(super) async fn sweep_orphans(
        &self,
        candidates: &mut HashMap<Pid, Candidate>,
        grace: Duration,
        max_defer: Duration,
    ) {
        // Most children get reaped by their owners right away, without a
        // zombie left the scan is not needed.
        if !has_zombie() {
            candidates.clear();
            return;
        }
        let (zombies, complete) = match task::spawn_blocking(zombie_children).await {
            Ok(zombies) => zombies,
            Err(e) => {
                error!("Unable to scan for zombie children: {e}");
                return;
            }
        };
        // The children of a thread which exited meanwhile got another thread
        // as parent, which may have been listed already. The next sweep finds
        // them.
        if !complete {
            self.sweep.notify_one();
        }

        let now = Instant::now();
        candidates.retain(|pid, _| zombies.contains(pid));
        // The owner of a claimed zombie reaps it soon, it needs no recheck.
        let claimed = self.claimed();
        // After the scan, a call which started later did not start the zombie.
        let calls_before = self.in_flight.borrow().next;
        for pid in zombies {
            if claimed.contains(&pid) {
                candidates.remove(&pid);
                continue;
            }
            let candidate = *candidates.entry(pid).or_insert(Candidate {
                first_seen: now,
                calls_before,
            });
            if now.duration_since(candidate.first_seen) < grace
                || self.defers(candidate, now, max_defer)
            {
                continue;
            }
            // Checked right before reaping, the sets change while the scan
            // runs. After the calls: a call which claimed the zombie
            // meanwhile is either still in progress or registered it.
            if self.is_claimed(pid) {
                candidates.remove(&pid);
                continue;
            }
            let reaping = reap_guard();
            let status = waitpid(pid, Some(WaitPidFlag::WNOHANG));
            drop(reaping);
            match status {
                Ok(status) => debug!("Reaped orphaned child {pid}: {status:?}"),
                Err(e) => debug!("Unable to reap orphaned child {pid}: {e}"),
            }
            candidates.remove(&pid);
        }
    }

    /// Whether the sweep leaves the zombie `candidate` alone at `now` for a
    /// call which may still claim it, at most for `max_defer` since it was
    /// first seen, see [`ChildReaper::ORPHAN_MAX_DEFER`].
    pub(super) fn defers(&self, candidate: Candidate, now: Instant, max_defer: Duration) -> bool {
        !self.shutting_down.load(Ordering::SeqCst)
            && now.duration_since(candidate.first_seen) < max_defer
            && self
                .in_flight
                .borrow()
                .may_claim_before(candidate.calls_before, now)
    }

    /// Whether runtime calls are in progress.
    #[cfg(test)]
    pub(super) fn calls_in_progress(&self) -> bool {
        !self.in_flight.borrow().in_progress.is_empty()
    }

    /// Whether a watcher or a call in progress waits for the child.
    ///
    /// Both sets get checked under the lock of the grandchildren, which
    /// `watch_grandchild` holds while it moves a child from one to the other.
    pub(super) fn is_claimed(&self, pid: Pid) -> bool {
        let map = lock(&self.grandchildren);
        map.values().flatten().any(|child| child.pid == pid)
            || lock(&self.pending).contains_key(&pid)
    }

    /// The children which a watcher or a call in progress waits for, see
    /// [`ChildReaper::is_claimed`].
    fn claimed(&self) -> HashSet<Pid> {
        let map = lock(&self.grandchildren);
        let mut claimed: HashSet<_> = map.values().flatten().map(|child| child.pid).collect();
        claimed.extend(lock(&self.pending).keys());
        claimed
    }
}
