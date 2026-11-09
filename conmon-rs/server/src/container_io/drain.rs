//! Bounding the output which gets forwarded after the process exited.

use super::Message;
use crate::{
    attach::SharedContainerAttach,
    channel::{Receiver, RecvError},
};
use std::ops::ControlFlow;
use tokio::{
    select,
    sync::broadcast,
    time::{self, Duration, Instant},
};
use tracing::{debug, warn};

/// What [`Drain::recv_logging`] ended with.
enum Logging {
    /// A result of the receiver, output which came meanwhile.
    Output(Result<Message, broadcast::error::RecvError>),

    /// No read loop holds output on its way into the log anymore.
    Logged,

    /// The log drivers did not take the output within the budget.
    GaveUp,
}

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

/// Upper bound for the time the exit files and the attach drains wait for
/// the log drivers to take the output after the process exited, which does
/// not count as a pause of the output, see `SharedContainerAttach::logging`
/// and [`Drain::recv_attach`], and for the read loops to end once stopped
/// and the log to sync, see `ReapableChild::finish_output`. A log driver
/// which is slow, like a journal which takes only part of every chunk in
/// time, or which hangs, like a log file on a stuck disk, must not keep the
/// exit from being reported for minutes.
pub(crate) const DRAIN_MAX_LOG_WAIT: Duration = Duration::from_secs(5);

/// Bounds forwarding the output after the process exited.
pub(crate) struct Drain {
    remaining: usize,
    waited: Duration,

    /// How long the drain waited for the log drivers so far, see
    /// [`Drain::recv_attach`].
    log_waited: Duration,
}

impl Default for Drain {
    fn default() -> Self {
        Self {
            remaining: DRAIN_MAX_MESSAGES,
            waited: Duration::ZERO,
            log_waited: Duration::ZERO,
        }
    }
}

impl Drain {
    /// Wait for the next message, giving up once the channel stays quiet for
    /// [`DRAIN_TIMEOUT`], [`DRAIN_MAX_MESSAGES`] were received or the waits
    /// took [`DRAIN_MAX_WAIT`].
    pub(crate) async fn recv<F: Future>(&mut self, recv: F) -> Option<F::Output> {
        if !self.take() {
            return None;
        }
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

    /// Account for the next message, false once [`DRAIN_MAX_MESSAGES`] got
    /// forwarded, which gets logged here.
    fn take(&mut self) -> bool {
        match self.remaining.checked_sub(1) {
            Some(remaining) => {
                self.remaining = remaining;
                true
            }
            None => {
                warn!(
                    "Dropping remaining output after forwarding {DRAIN_MAX_MESSAGES} messages since the exit"
                );
                false
            }
        }
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
            let res = self.recv(rx.recv()).await?;
            if let ControlFlow::Break(message) = Self::output(res, rx, lag.as_deref_mut()) {
                return message;
            }
        }
    }

    /// What a result of the attach receiver `rx` means for the client, see
    /// [`Drain::recv_output`]: the message, none once the output ended, or
    /// skipped messages, which `lag` notes and the caller goes on after.
    fn output(
        res: Result<Message, broadcast::error::RecvError>,
        rx: &broadcast::Receiver<Message>,
        lag: Option<&mut Lag>,
    ) -> ControlFlow<Option<Message>> {
        match res {
            Ok(Message::Done) | Err(broadcast::error::RecvError::Closed) => {
                ControlFlow::Break(None)
            }
            Ok(message) => {
                if let Some(lag) = lag {
                    lag.received(rx);
                }
                ControlFlow::Break(Some(message))
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                match lag {
                    Some(lag) => lag.skipped(n),
                    None => debug!("Skipped {n} attach output messages"),
                }
                ControlFlow::Continue(())
            }
        }
    }

    /// The next message of the attach output of `attach` like
    /// [`Drain::recv_output`], for a drain after the exit. A read loop which
    /// holds output on its way into the log, see
    /// [`SharedContainerAttach::logging`], gets until [`DRAIN_MAX_LOG_WAIT`]
    /// in total to broadcast it, which is no pause of the output: the client
    /// must not lose the last output to a log write which takes longer than
    /// [`DRAIN_TIMEOUT`]. The output which got broadcast already, or gets
    /// broadcast meanwhile by another read loop, comes right away, a log
    /// which hangs must not keep it from the client. None once the output
    /// ended or the drain gave up.
    pub(crate) async fn recv_attach(
        &mut self,
        attach: &SharedContainerAttach,
        rx: &mut broadcast::Receiver<Message>,
        mut lag: Option<&mut Lag>,
    ) -> Option<Message> {
        loop {
            let res = if attach.is_logging() {
                match self.recv_logging(attach, rx).await {
                    Logging::Output(res) => res,
                    // Taken by the log, the output gets broadcast now.
                    Logging::Logged => continue,
                    Logging::GaveUp => return None,
                }
            } else {
                let Some(res) = self.recv(rx.recv()).await else {
                    // A read loop which got output while the drain waited
                    // gives it another chance.
                    if self.exhausted() || !attach.is_logging() {
                        return None;
                    }
                    continue;
                };
                res
            };
            if let ControlFlow::Break(message) = Self::output(res, rx, lag.as_deref_mut()) {
                return message;
            }
        }
    }

    /// Receive from `rx` while a read loop holds output on its way into the
    /// log, until it took it or what is left of [`DRAIN_MAX_LOG_WAIT`]
    /// passed, which gets logged here once. The output which is queued or
    /// gets broadcast meanwhile comes first.
    async fn recv_logging(
        &mut self,
        attach: &SharedContainerAttach,
        rx: &mut broadcast::Receiver<Message>,
    ) -> Logging {
        if !self.take() {
            return Logging::GaveUp;
        }
        let budget = DRAIN_MAX_LOG_WAIT.saturating_sub(self.log_waited);
        let start = Instant::now();
        let res = select! {
            biased;
            res = rx.recv() => Logging::Output(res),
            logged = time::timeout(budget, attach.wait_logged()) => match logged {
                Ok(()) => {
                    // No message got taken, the slot goes back.
                    self.remaining += 1;
                    Logging::Logged
                }
                Err(_) => {
                    // Logged once, when the budget ran out.
                    if !budget.is_zero() {
                        warn!("Log drivers did not take the output in time, not waiting for them");
                    }
                    Logging::GaveUp
                }
            },
        };
        self.log_waited += start.elapsed();
        res
    }

    /// The next message of a stdio channel with [`Drain::recv`]. None once
    /// the output ended, the channel got closed or the drain gave up.
    pub(crate) async fn recv_stdio(&mut self, rx: &Receiver<Message>) -> Option<Message> {
        match self.recv(rx.recv()).await? {
            Ok(Message::Done) | Err(RecvError) => None,
            Ok(message) => Some(message),
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
