//! The idle timeout of streaming connections.

use std::{
    future,
    sync::{Arc, Mutex},
};
use tokio::time::{self, Duration, Instant};

/// When a connection was active the last time. Clones share the time.
#[derive(Clone, Debug)]
pub struct Activity(Arc<Mutex<Instant>>);

impl Activity {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    /// The connection is active now.
    pub fn touch(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    fn last(&self) -> Instant {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
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
