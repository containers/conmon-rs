//! A bounded channel with any number of senders and receivers, for the stdio
//! channels of the containers and the stdin of their attach clients.
//!
//! Unlike the mpsc channel of tokio, its receivers can be cloned and closed
//! through a shared reference, and a sender can push out the oldest message
//! of a full channel instead of waiting, see [`Sender::force_send`]. The
//! messages stay in the order they got sent, every message goes to one of
//! the receivers.

use crate::mutex::lock;
use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

/// Create a channel which holds up to `capacity` messages, which must not be
/// zero.
pub(crate) fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0, "channel capacity must not be zero");
    let channel = Arc::new(Channel {
        state: Mutex::new(State {
            queue: VecDeque::new(),
            closed: false,
            senders: 1,
            receivers: 1,
        }),
        capacity,
        readable: Notify::new(),
        writable: Notify::new(),
        closed: Notify::new(),
    });
    (Sender(channel.clone()), Receiver(channel))
}

/// The state shared by the senders and receivers of a channel.
struct Channel<T> {
    state: Mutex<State<T>>,
    capacity: usize,

    /// Wakes the receivers once a message got queued into an empty channel,
    /// or the channel got closed. A receiver waits only while the channel is
    /// empty, so the other messages need no wake-up. That makes it wake all
    /// waiters, the messages queued before they ran are for them as well.
    readable: Notify,

    /// Wakes the senders once a message got taken from a full channel, or
    /// the channel got closed. A sender waits only while the channel is
    /// full, all waiters get woken, see `readable`.
    writable: Notify,

    /// Wakes [`Sender::closed`] once the channel got closed, which a busy
    /// channel would wake up for every message otherwise.
    closed: Notify,
}

struct State<T> {
    /// Grows on demand, an idle channel holds no buffer of its capacity.
    queue: VecDeque<T>,

    /// Set by [`Receiver::close`].
    closed: bool,
    senders: usize,
    receivers: usize,
}

impl<T> State<T> {
    /// Whether nothing can be sent anymore. The queued messages can still
    /// be received.
    fn is_closed(&self) -> bool {
        self.closed || self.senders == 0 || self.receivers == 0
    }
}

impl<T> Channel<T> {
    /// Queue a message. A full channel fails, or drops its oldest message
    /// with `force`.
    fn push(&self, message: T, force: bool) -> Result<(), Push<T>> {
        let mut state = lock(&self.state);
        if state.is_closed() {
            return Err(Push::Closed(message));
        }
        let dropped = if state.queue.len() < self.capacity {
            None
        } else if force {
            state.queue.pop_front()
        } else {
            return Err(Push::Full(message));
        };
        let was_empty = state.queue.is_empty();
        state.queue.push_back(message);
        drop(state);
        drop(dropped);
        if was_empty {
            self.readable.notify_waiters();
        }
        Ok(())
    }

    /// Take the oldest message.
    fn pop(&self) -> Result<T, TryRecvError> {
        let mut state = lock(&self.state);
        let was_full = state.queue.len() >= self.capacity;
        match state.queue.pop_front() {
            Some(message) => {
                drop(state);
                if was_full {
                    self.writable.notify_waiters();
                }
                Ok(message)
            }
            None if state.is_closed() => Err(TryRecvError::Closed),
            None => Err(TryRecvError::Empty),
        }
    }

    /// Wake everything which waits, after the channel got closed.
    fn wake_all(&self) {
        self.readable.notify_waiters();
        self.writable.notify_waiters();
        self.closed.notify_waiters();
    }
}

/// Why a message could not be queued.
enum Push<T> {
    Full(T),
    Closed(T),
}

/// The sending half of a channel.
pub(crate) struct Sender<T>(Arc<Channel<T>>);

impl<T> Sender<T> {
    /// Send a message, waiting while the channel is full. Fails with the
    /// message once the channel got closed or all receivers are gone.
    pub(crate) async fn send(&self, mut message: T) -> Result<(), SendError<T>> {
        loop {
            // Created before the check, so that a receiver which makes room
            // right after it wakes this one up. Only notify_waiters wakes
            // the channel's waiters, which a Notified sees from its
            // creation on.
            let writable = self.0.writable.notified();
            match self.0.push(message, false) {
                Ok(()) => return Ok(()),
                Err(Push::Closed(message)) => return Err(SendError(message)),
                Err(Push::Full(full)) => message = full,
            }
            writable.await;
        }
    }

    /// Send a message without waiting, a full channel drops its oldest
    /// message for it. Fails with the message once the channel got closed or
    /// all receivers are gone.
    pub(crate) fn force_send(&self, message: T) -> Result<(), SendError<T>> {
        match self.0.push(message, true) {
            Ok(()) => Ok(()),
            Err(Push::Closed(message) | Push::Full(message)) => Err(SendError(message)),
        }
    }

    /// Complete once the channel got closed or all receivers are gone.
    pub(crate) async fn closed(&self) {
        loop {
            // Created before the check, see Sender::send.
            let closed = self.0.closed.notified();
            if lock(&self.0.state).is_closed() {
                return;
            }
            closed.await;
        }
    }

    /// The number of queued messages.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        lock(&self.0.state).queue.len()
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        lock(&self.0.state).senders += 1;
        Self(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.senders -= 1;
        let last = state.senders == 0;
        drop(state);
        if last {
            self.0.wake_all();
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

/// The receiving half of a channel, whose clones share the messages.
pub(crate) struct Receiver<T>(Arc<Channel<T>>);

impl<T> Receiver<T> {
    /// Receive the oldest message, waiting while the channel is empty. Fails
    /// once the channel is empty and got closed or all senders are gone.
    pub(crate) async fn recv(&self) -> Result<T, RecvError> {
        loop {
            // Created before the check, see Sender::send.
            let readable = self.0.readable.notified();
            match self.0.pop() {
                Ok(message) => return Ok(message),
                Err(TryRecvError::Closed) => return Err(RecvError),
                Err(TryRecvError::Empty) => readable.await,
            }
        }
    }

    /// Receive the oldest message without waiting.
    pub(crate) fn try_recv(&self) -> Result<T, TryRecvError> {
        self.0.pop()
    }

    /// Close the channel for all senders and receivers. Sending fails from
    /// now on, waiting senders give up, the queued messages can still be
    /// received.
    pub(crate) fn close(&self) {
        lock(&self.0.state).closed = true;
        self.0.wake_all();
    }

    /// Whether the channel got closed, or all senders are gone.
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        lock(&self.0.state).is_closed()
    }

    /// The number of senders.
    #[cfg(test)]
    pub(crate) fn sender_count(&self) -> usize {
        lock(&self.0.state).senders
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        lock(&self.0.state).receivers += 1;
        Self(self.0.clone())
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.receivers -= 1;
        if state.receivers > 0 {
            return;
        }
        // Nothing can receive the queued messages anymore, the senders may
        // keep the channel around for long.
        let queue = std::mem::take(&mut state.queue);
        drop(state);
        drop(queue);
        self.0.wake_all();
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver").finish_non_exhaustive()
    }
}

/// The channel got closed or all receivers are gone, with the message which
/// could not be sent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SendError<T>(pub(crate) T);

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sending into a closed channel")
    }
}

impl<T: fmt::Debug> Error for SendError<T> {}

/// The channel is empty and got closed, or all senders are gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("receiving from an empty and closed channel")
    }
}

impl Error for RecvError {}

/// Why [`Receiver::try_recv`] got no message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TryRecvError {
    /// The channel is empty.
    Empty,

    /// The channel is empty and got closed, or all senders are gone.
    Closed,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("receiving from an empty channel"),
            Self::Closed => RecvError.fmt(f),
        }
    }
}

impl Error for TryRecvError {}

#[cfg(test)]
mod tests;
