//! Bounding the output which gets forwarded after the process exited.

use super::Message;
use tokio::{
    sync::broadcast,
    time::{self, Duration, Instant},
};
use tracing::{debug, warn};

/// Upper bound for waiting on the next output after the process exited. The
/// read loops may still be draining the pipes when the exit is observed, they
/// signal the end with `Message::Done`. The timeout only kicks in if that
/// never arrives, for example because a grandchild keeps the pipe open.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

/// Upper bound for the messages forwarded per drain after the process exited.
/// The output written before the exit is at most what the channels (up to
/// 128 messages) and the pipe buffers hold. With the default pipe-max-size of
/// 1 MiB in 8 KiB reads, that is 128 messages per pipe, and small writes
/// read one by one do not fill a pipe of that size either, so this is well
/// above. It is a heuristic though, a process with CAP_SYS_RESOURCE can grow
/// its pipes further. More output stems from a grandchild which keeps the
/// pipe open and writing, which would otherwise keep the drain going forever.
pub(crate) const DRAIN_MAX_MESSAGES: usize = 1024;

/// Upper bound for the time a drain waits for output after the process
/// exited in total. A grandchild which keeps writing just often enough for
/// [`DRAIN_TIMEOUT`] would keep it going for minutes otherwise. Only the
/// waiting counts, not the forwarding, so a slow client still gets the
/// output which was there already.
pub(crate) const DRAIN_MAX_WAIT: Duration = Duration::from_secs(1);

/// Upper bound for the time the exit files wait for the log drivers to take
/// the output after the process exited, which does not count as a pause of
/// the output, see `SharedContainerAttach::logging`, and for the read loops
/// to end once stopped and the log to sync, see
/// `ReapableChild::finish_output`. A log driver which is slow, like a
/// journal which takes only part of every chunk in time, or which hangs,
/// like a log file on a stuck disk, must not keep the exit from being
/// reported for minutes.
pub(crate) const DRAIN_MAX_LOG_WAIT: Duration = Duration::from_secs(5);

/// Bounds forwarding the output after the process exited.
pub(crate) struct Drain {
    remaining: usize,
    waited: Duration,
}

impl Default for Drain {
    fn default() -> Self {
        Self {
            remaining: DRAIN_MAX_MESSAGES,
            waited: Duration::ZERO,
        }
    }
}

impl Drain {
    /// Wait for the next message, giving up once the channel stays quiet for
    /// [`DRAIN_TIMEOUT`], [`DRAIN_MAX_MESSAGES`] were received or the waits
    /// took [`DRAIN_MAX_WAIT`].
    pub(crate) async fn recv<F: Future>(&mut self, recv: F) -> Option<F::Output> {
        let Some(remaining) = self.remaining.checked_sub(1) else {
            warn!(
                "Dropping remaining output after forwarding {DRAIN_MAX_MESSAGES} messages since the exit"
            );
            return None;
        };
        self.remaining = remaining;
        let budget = DRAIN_MAX_WAIT.saturating_sub(self.waited);
        if budget.is_zero() {
            debug!("Dropping remaining output after waiting {DRAIN_MAX_WAIT:?} since the exit");
            return None;
        }
        let start = Instant::now();
        let res = time::timeout(DRAIN_TIMEOUT.min(budget), recv).await.ok();
        self.waited += start.elapsed();
        res
    }

    /// Whether the drain forwarded or waited as much as it may.
    pub(crate) fn exhausted(&self) -> bool {
        self.remaining == 0 || self.waited >= DRAIN_MAX_WAIT
    }

    /// The next message of the attach output with [`Drain::recv`]. A
    /// receiver which fell behind skips messages, which gets logged, for
    /// the receiver of a client with its [`Lag`], the one it kept before
    /// the exit. None once the output ended or the drain gave up.
    pub(crate) async fn recv_output(
        &mut self,
        rx: &mut broadcast::Receiver<Message>,
        mut lag: Option<&mut Lag>,
    ) -> Option<Message> {
        loop {
            match self.recv(rx.recv()).await? {
                Ok(Message::Done) | Err(broadcast::error::RecvError::Closed) => return None,
                Ok(message) => {
                    if let Some(lag) = lag.as_deref_mut() {
                        lag.received(rx);
                    }
                    return Some(message);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => match lag.as_deref_mut() {
                    Some(lag) => lag.skipped(n),
                    None => debug!("Skipped {n} attach output messages"),
                },
            }
        }
    }
}

/// Logs an attach client which falls behind the output once until it
/// caught up again, like [`LogErrors`]. A client which stays too slow skips
/// messages again every time the channel filled up, which would log a
/// warning for every one of its capacity of messages otherwise.
///
/// [`LogErrors`]: super::read_loop::LogErrors
#[derive(Debug, Default)]
pub(crate) struct Lag {
    /// The messages skipped since the client fell behind, none while it
    /// keeps up.
    pub(super) skipped: Option<u64>,
}

impl Lag {
    /// The client skipped `n` messages.
    pub(crate) fn skipped(&mut self, n: u64) {
        match &mut self.skipped {
            Some(skipped) => {
                *skipped = skipped.saturating_add(n);
                debug!("Attach client still too slow, skipped {skipped} messages so far");
            }
            None => {
                self.skipped = Some(n);
                warn!("Attach client too slow, skipped {n} messages, more until it catches up");
            }
        }
    }

    /// The client received a message from `rx`. It caught up once nothing
    /// is queued for it anymore.
    pub(crate) fn received<T>(&mut self, rx: &broadcast::Receiver<T>) {
        if self.skipped.is_some()
            && rx.is_empty()
            && let Some(skipped) = self.skipped.take()
        {
            debug!("Attach client caught up after skipping {skipped} messages");
        }
    }
}
