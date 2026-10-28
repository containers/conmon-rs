//! Creating a value of which the server has a single one once, on demand,
//! and undoing it on shutdown.

use crate::{mutex::lock, process::Process};
use anyhow::{Context, Result, bail, format_err};
use nix::sys::signal::Signal;
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    sync::{OnceCell, watch},
    task,
    time::{self, Duration, Instant},
};
use tracing::{Instrument, Span, warn};

/// A value which has to be undone when it is no longer used.
pub trait Stop {
    /// Undo the value, which may block, see [`stop_blocking`].
    fn stop(&self);
}

/// Run the stop of a value on the blocking pool. Unmounting waits for the
/// kernel, which can take long, the runtime thread runs every RPC and the
/// I/O of all containers.
pub async fn stop_blocking(stop: impl FnOnce() + Send + 'static) {
    if let Err(e) = task::spawn_blocking(stop).await {
        warn!("Unable to stop: {e}");
    }
}

/// Creates a value of which a server has a single one, like the pause
/// instance, once and on demand.
///
/// Only one initialization runs at a time, requests which arrive meanwhile
/// wait for it instead of queueing their own. It runs in a task of its own,
/// so a request cancelled meanwhile, for example by a CRI-O timeout, cannot
/// abandon a half set up value. A failed or panicked initialization leaves
/// the cell empty, so the next request starts over.
pub struct SingleFlight<T> {
    cell: OnceCell<T>,

    /// Whether an initialization runs.
    running: LazyLock<watch::Sender<bool>>,

    /// The process of the running initialization, so that the shutdown can
    /// kill it if it does not finish in time.
    child: Mutex<Option<Process>>,

    /// Set once the server shuts down. No initialization starts after that
    /// and one which is still running stops its value itself, so that the
    /// shutdown does not leave one behind.
    shutting_down: AtomicBool,
}

/// Marks the initialization as finished, whatever happens to its task.
struct InitRunning<'a>(&'a watch::Sender<bool>);

impl Drop for InitRunning<'_> {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
}

impl<T: Stop + Send + Sync + 'static> SingleFlight<T> {
    pub const fn new() -> Self {
        Self {
            cell: OnceCell::const_new(),
            running: LazyLock::new(Self::not_running),
            child: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
        }
    }

    fn not_running() -> watch::Sender<bool> {
        watch::Sender::new(false)
    }

    pub fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Retrieve the value, running `init` if there is none and no other
    /// initialization runs. A request which waited for the initialization of
    /// another one fails if that failed, its outcome goes to the request
    /// which started it and a retry runs a new one.
    pub async fn get_or_init<F, Fut>(&'static self, init: F) -> Result<&'static T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send,
    {
        if self.shutting_down() {
            bail!("server is shutting down")
        }

        if self.cell.get().is_none() {
            // Claim the initialization if none runs.
            let claimed = self.running.send_if_modified(|running| {
                let claimed = !*running;
                *running = true;
                claimed
            });

            if claimed {
                let init = task::spawn(
                    async move {
                        let _running = InitRunning(&self.running);
                        self.cell
                            .get_or_try_init(|| self.init_unless_shutting_down(init))
                            .await
                            .map(drop)
                    }
                    .instrument(Span::current()),
                );
                init.await.context("join initialization")??;
            } else {
                self.wait_for_init().await;
            }
        }

        self.cell
            .get()
            .context("the initialization waited for failed, retry")
    }

    /// Wait until no initialization runs.
    async fn wait_for_init(&self) {
        // Only fails if the sender is gone, which lives as long as self.
        let _ = self.running.subscribe().wait_for(|running| !*running).await;
    }

    /// Run the initialization, unless the server shuts down. If the shutdown
    /// started meanwhile it may have given up waiting for this, so stop the
    /// value here instead of publishing it.
    async fn init_unless_shutting_down<F, Fut>(&self, init: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        if self.shutting_down() {
            bail!("server is shutting down")
        }
        let value = init().await?;
        if self.shutting_down() {
            stop_blocking(move || value.stop()).await;
            bail!("server is shutting down")
        }
        Ok(value)
    }

    /// Set the process of the running initialization.
    pub fn set_child(&self, child: Option<Process>) {
        *lock(&self.child) = child;
    }

    /// Stop the value for the shutdown, until the `deadline`. Wait for a
    /// running initialization until `kill_wait` and `stop_wait` before it,
    /// kill its process if it does not finish and wait until `stop_wait`
    /// before the deadline for it to clean up, then for the stop of the
    /// value. No request initializes one after this.
    pub async fn shutdown(
        &'static self,
        deadline: Instant,
        kill_wait: Duration,
        stop_wait: Duration,
    ) -> Result<()> {
        self.shutting_down.store(true, Ordering::SeqCst);
        let stop_deadline = deadline.checked_sub(stop_wait).unwrap_or(deadline);
        let kill_deadline = stop_deadline
            .checked_sub(kill_wait)
            .unwrap_or(stop_deadline);

        let mut result = Ok(());
        if time::timeout_at(kill_deadline, self.wait_for_init())
            .await
            .is_err()
        {
            warn!("Initialization still running, killing its process");
            if let Some(child) = &*lock(&self.child)
                && let Err(e) = child.signal(Signal::SIGKILL)
            {
                warn!(
                    "Unable to kill initialization process {}: {:#}",
                    child.pid(),
                    e
                );
            }
            if time::timeout_at(stop_deadline, self.wait_for_init())
                .await
                .is_err()
            {
                result = Err(format_err!(
                    "initialization did not finish, its process may stay behind"
                ));
            }
        }

        // A stop which hangs, like an unmount on a hung file system, must
        // not keep the server from stopping.
        if let Some(value) = self.cell.get()
            && time::timeout_at(deadline, stop_blocking(move || value.stop()))
                .await
                .is_err()
        {
            result = Err(format_err!("stop did not finish in time"));
        }
        result
    }
}

#[cfg(test)]
mod tests;
