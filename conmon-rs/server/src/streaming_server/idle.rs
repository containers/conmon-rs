//! The idle timeout of streaming connections.

use crate::mutex::lock;
use std::{
    future::{self, Future},
    sync::{Arc, Mutex},
};
use tokio::{
    select,
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// How long the end of a connection waits for a client which takes nothing
/// of what the server sends, like the rest of the output, the status of a
/// session and the close, before the connection gets ended. A slow client
/// gets all of it, only one which stopped reading gets dropped, see
/// [`Activity::until_stalled`]. Not too short: the kernel wakes up a write
/// which waits for the client only once a third of the send buffer drained,
/// which can take a while with a send buffer of a few MiB and a slow
/// client.
pub const STALL_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(5)
} else {
    Duration::from_secs(60)
};

/// When a connection was active the last time, by what the client sent or
/// got. Clones share the time.
#[derive(Clone, Debug)]
pub struct Activity(Arc<Mutex<Instant>>);

impl Activity {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    /// The connection is active now.
    pub fn touch(&self) {
        *lock(&self.0) = Instant::now();
    }

    fn last(&self) -> Instant {
        *lock(&self.0)
    }

    /// Resolves once the connection was not active for the timeout. A timeout
    /// too large to represent is none, like no timeout.
    pub async fn idle(&self, timeout: Option<Duration>) {
        let Some(timeout) = timeout else {
            return future::pending().await;
        };
        loop {
            let Some(deadline) = self.last().checked_add(timeout) else {
                return future::pending().await;
            };
            if Instant::now() >= deadline {
                return;
            }
            time::sleep_until(deadline).await;
        }
    }

    /// Run `run` until it is done, or until the connection was not active
    /// for the timeout, which gives `None`. `on_idle` runs before `run` gets
    /// dropped then.
    pub async fn until_idle<T>(
        &self,
        timeout: Option<Duration>,
        run: impl Future<Output = T>,
        on_idle: impl FnOnce(),
    ) -> Option<T> {
        let idle = async {
            self.idle(timeout).await;
            on_idle();
        };
        select! {
            output = run => Some(output),
            () = idle => None,
        }
    }

    /// Run `run` until it is done, or until the activity was not touched
    /// for the `timeout`, counted from `start` at the earliest, which gives
    /// `None`. The writes of a connection touch it as they go out, which
    /// lets the end of a connection take as long as a slow client needs,
    /// see [`STALL_TIMEOUT`].
    pub async fn until_stalled<T>(
        &self,
        start: Instant,
        timeout: Duration,
        run: impl Future<Output = T>,
    ) -> Option<T> {
        self.until_stalled_or_idle(start, timeout, None, run).await
    }

    /// Like [`Self::until_stalled`], but not before the activity was not
    /// touched for the idle timeout either. A client which pauses the output,
    /// like a pager, keeps the end as long as it kept the rest of a session,
    /// one which ended for its idle timeout gets only the `timeout`, like
    /// with an idle timeout too large for the clock.
    pub async fn until_stalled_or_idle<T>(
        &self,
        start: Instant,
        timeout: Duration,
        idle_timeout: Option<Duration>,
        run: impl Future<Output = T>,
    ) -> Option<T> {
        let deadline = |last: Instant| {
            let stalled = last.max(start).checked_add(timeout)?;
            let idle = idle_timeout.and_then(|idle_timeout| last.checked_add(idle_timeout));
            Some(idle.map_or(stalled, |idle| stalled.max(idle)))
        };
        let stalled = async {
            loop {
                let Some(deadline) = deadline(self.last()) else {
                    return future::pending().await;
                };
                if Instant::now() >= deadline {
                    return;
                }
                time::sleep_until(deadline).await;
            }
        };
        select! {
            biased;
            output = run => Some(output),
            () = stalled => None,
        }
    }

    /// Watch the connection in a task of its own until `ended` gets
    /// cancelled, and run `on_idle` once it was not active for the timeout
    /// before that.
    pub fn watch(
        &self,
        timeout: Option<Duration>,
        ended: CancellationToken,
        on_idle: impl FnOnce() + Send + 'static,
    ) {
        let activity = self.clone();
        tokio::spawn(async move {
            if activity
                .until_idle(timeout, ended.cancelled(), || {})
                .await
                .is_none()
            {
                on_idle();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn idle_after_the_timeout() {
        let activity = Activity::new();
        let timeout = Some(Duration::from_secs(60));
        time::sleep(Duration::from_secs(50)).await;
        activity.touch();
        assert!(
            time::timeout(Duration::from_secs(59), activity.idle(timeout))
                .await
                .is_err()
        );
        time::timeout(Duration::from_secs(2), activity.idle(timeout))
            .await
            .expect("idle");
    }

    #[tokio::test(start_paused = true)]
    async fn runs_end_with_the_idle_timeout() {
        let activity = Activity::new();
        let timeout = Some(Duration::from_secs(60));
        let done = activity
            .until_idle(timeout, time::sleep(Duration::from_secs(30)), || {
                panic!("idle")
            })
            .await;
        assert_eq!(done, Some(()));

        // The run sees the idle callback before it gets dropped.
        let idle = Arc::new(Mutex::new(false));
        struct Check(Arc<Mutex<bool>>);
        impl Drop for Check {
            fn drop(&mut self) {
                assert!(*self.0.lock().unwrap());
            }
        }
        let check = Check(idle.clone());
        let run = async move {
            let _check = check;
            future::pending::<()>().await;
        };
        let start = Instant::now();
        let result = activity
            .until_idle(timeout, run, || *idle.lock().unwrap() = true)
            .await;
        assert_eq!(result, None);
        assert_eq!(start.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn only_stalls_end_runs() {
        let activity = Activity::new();
        let timeout = Duration::from_secs(5);
        // An activity from long ago does not count against a run which
        // starts now.
        time::sleep(Duration::from_secs(60)).await;
        let start = Instant::now();
        let progress = async {
            for _ in 0..10 {
                time::sleep(Duration::from_secs(4)).await;
                activity.touch();
            }
        };
        assert_eq!(
            activity.until_stalled(start, timeout, progress).await,
            Some(())
        );
        assert_eq!(start.elapsed(), Duration::from_secs(40));

        // A run which stops making progress ends after the timeout.
        let start = Instant::now();
        let stalls = async {
            time::sleep(Duration::from_secs(3)).await;
            activity.touch();
            future::pending::<()>().await;
        };
        assert_eq!(activity.until_stalled(start, timeout, stalls).await, None);
        assert_eq!(start.elapsed(), Duration::from_secs(8));
    }

    #[tokio::test(start_paused = true)]
    async fn stalls_end_runs_after_the_idle_timeout() {
        let activity = Activity::new();
        let timeout = Duration::from_secs(5);
        let idle_timeout = Some(Duration::from_secs(60));

        // A client which got something 20 seconds ago, then paused, gets the
        // rest of the idle timeout.
        time::sleep(Duration::from_secs(20)).await;
        let start = Instant::now();
        let stalls = future::pending::<()>();
        assert_eq!(
            activity
                .until_stalled_or_idle(start, timeout, idle_timeout, stalls)
                .await,
            None
        );
        assert_eq!(start.elapsed(), Duration::from_secs(40));

        // One which got nothing for the idle timeout still gets the timeout.
        let start = Instant::now();
        let stalls = future::pending::<()>();
        assert_eq!(
            activity
                .until_stalled_or_idle(start, timeout, idle_timeout, stalls)
                .await,
            None
        );
        assert_eq!(start.elapsed(), timeout);

        // Each part which goes out gives it the idle timeout again.
        activity.touch();
        let start = Instant::now();
        let progress = async {
            for _ in 0..3 {
                time::sleep(Duration::from_secs(50)).await;
                activity.touch();
            }
        };
        assert_eq!(
            activity
                .until_stalled_or_idle(start, timeout, idle_timeout, progress)
                .await,
            Some(())
        );
        assert_eq!(start.elapsed(), Duration::from_secs(150));
    }

    #[tokio::test(start_paused = true)]
    async fn huge_idle_timeouts_leave_the_stall_timeout() {
        let activity = Activity::new();
        let timeout = Duration::from_secs(5);
        let start = Instant::now();
        let stalls = future::pending::<()>();
        let ended = activity.until_stalled_or_idle(start, timeout, Some(Duration::MAX), stalls);
        assert_eq!(time::timeout(10 * timeout, ended).await, Ok(None));
        assert_eq!(start.elapsed(), timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn huge_timeouts_are_none() {
        let activity = Activity::new();
        for timeout in [
            None,
            Some(Duration::MAX),
            Some(Duration::from_secs(u64::MAX)),
        ] {
            assert!(
                time::timeout(Duration::from_secs(3600), activity.idle(timeout))
                    .await
                    .is_err()
            );
        }
    }
}
