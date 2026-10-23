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
