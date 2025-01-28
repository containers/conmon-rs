use std::{
    future, process,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::sync::{futures::Notified, Notify};

/// Track activity and reacto to inactivity.
///
/// Can be used to exit accept loops after inactivity.
#[derive(Debug, Clone)]
pub struct Inactivity(Option<Arc<Inner>>);

impl Inactivity {
    /// Create a new inactivity tracker.
    pub fn new() -> Self {
        Self(Some(Arc::default()))
    }

    /// Create a disabled inactivity tracker.
    ///
    /// The wait function will never return. There is always activity.
    pub const fn disabled() -> Self {
        Self(None)
    }

    /// Start tracking an activity.
    pub fn activity(&self) -> Activity {
        Activity::new(self.0.as_ref())
    }

    /// Async "block" until there is no activity and then wait for an additional`timeout`.
    pub async fn wait(&self, timeout: Duration) {
        if let Some(inner) = &self.0 {
            loop {
                let changed = inner.changed();
                if inner.no_activity() {
                    let _ = tokio::time::timeout(timeout, changed).await;
                    if inner.no_activity() {
                        break;
                    }
                } else {
                    changed.await
                }
            }
        } else {
            future::pending().await
        }
    }
}

/// Track an activity. Can be cloned to track additional activities.
///
/// The Activity stops on drop.
#[derive(Debug)]
pub struct Activity(Option<Arc<Inner>>);

impl Activity {
    fn new(inner: Option<&Arc<Inner>>) -> Self {
        Self(inner.map(Inner::increment))
    }

    /// Explicitly stop an activity. Just a wrapper for `drop(activity)`.
    pub fn stop(self) {
        // nothing to to, we take self by value
    }
}

impl Clone for Activity {
    fn clone(&self) -> Self {
        Self::new(self.0.as_ref())
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if let Some(inner) = &self.0 {
            inner.decrement();
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    // number of current activities
    active: AtomicUsize,
    // gets notofied whenever the result of `no_activity` changes
    notify: Notify,
}

impl Inner {
    /// Abort if more then isize::MAX activities are active.
    ///
    /// This prevents integer overflow of the `active` counter in case
    /// someone is `mem::forget`ing activities.
    ///
    /// The same logic is applied internally by the `Arc` implementation.
    const MAX_ACTIVE: usize = isize::MAX as usize;

    fn increment(self: &Arc<Self>) -> Arc<Self> {
        match self.active.fetch_add(1, Ordering::Relaxed) {
            0 => self.notify.notify_waiters(),
            1..=Self::MAX_ACTIVE => {}
            _ => process::abort(),
        }
        self.clone()
    }

    fn decrement(&self) {
        match self.active.fetch_sub(1, Ordering::Relaxed) {
            1 => self.notify.notify_waiters(),
            2..=Self::MAX_ACTIVE => {}
            _ => process::abort(),
        }
    }

    fn no_activity(&self) -> bool {
        self.active.load(Ordering::Relaxed) == 0
    }

    fn changed(&self) -> Notified {
        self.notify.notified()
    }
}
