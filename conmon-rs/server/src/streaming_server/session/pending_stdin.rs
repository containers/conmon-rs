//! The stdin of a client which waits for the container, see
//! [`PendingStdin`].

use super::PENDING_STDIN_MAX_BYTES;
use crate::streaming_server::remotecommand::Stdin;
use bytes::{Bytes, BytesMut};
use std::collections::VecDeque;

/// The largest part of stdin which goes towards the container at once.
/// Larger stdin gets split without copying it, so that the channels on the
/// way, which count entries, hold at most this much per entry.
pub(super) const STDIN_CHUNK_SIZE: usize = 64 * 1024;

/// Stdin smaller than this gets merged with the stdin before it while it
/// waits, so that a client which sends a byte per message does not make the
/// server hold far more than the bytes it counts.
pub(super) const STDIN_MERGE_SIZE: usize = 32 * 1024;

/// What an entry of the pending stdin counts on top of its bytes.
pub(super) const STDIN_ENTRY_OVERHEAD: usize = 64;

/// Stdin of a client which the container did not take yet, bounded by
/// [`PENDING_STDIN_MAX_BYTES`]. Small stdin gets merged and large stdin
/// split into entries of at most [`STDIN_CHUNK_SIZE`], each counts
/// [`STDIN_ENTRY_OVERHEAD`] on top of its bytes. Empty stdin carries nothing,
/// and nothing takes stdin after the close, both get dropped. The entries
/// of large stdin keep its message until the container took the last one,
/// stdin goes to the container in order, so only the oldest message is
/// partly taken.
#[derive(Debug, Default)]
pub(super) struct PendingStdin {
    /// Stdin of at least [`STDIN_MERGE_SIZE`] bytes and merged small stdin,
    /// the oldest first.
    chunks: VecDeque<Bytes>,

    /// Small stdin after the chunks, merged.
    tail: BytesMut,

    /// The bytes and the overhead of the chunks.
    bytes: usize,

    /// Whether the client closed its stdin.
    pub(super) closed: bool,

    /// Whether the close still has to be taken, after the stdin before it.
    close_pending: bool,
}

impl PendingStdin {
    pub(super) fn is_empty(&self) -> bool {
        self.chunks.is_empty() && self.tail.is_empty() && !self.close_pending
    }

    /// What the pending stdin counts against [`PENDING_STDIN_MAX_BYTES`].
    pub(super) fn len(&self) -> usize {
        let tail = if self.tail.is_empty() {
            0
        } else {
            self.tail.len() + STDIN_ENTRY_OVERHEAD
        };
        let close = if self.close_pending {
            STDIN_ENTRY_OVERHEAD
        } else {
            0
        };
        self.bytes + tail + close
    }

    /// Whether the client has to wait, which stops reading the socket.
    pub(super) fn is_full(&self) -> bool {
        self.len() >= PENDING_STDIN_MAX_BYTES
    }

    pub(super) fn push(&mut self, stdin: Stdin) {
        if self.closed {
            return;
        }
        match stdin {
            Stdin::Close => {
                self.closed = true;
                self.close_pending = true;
            }
            Stdin::Data(data) if data.len() < STDIN_MERGE_SIZE => {
                if self.tail.len() + data.len() > STDIN_CHUNK_SIZE {
                    self.flush_tail();
                }
                self.tail.extend_from_slice(&data);
            }
            Stdin::Data(mut data) => {
                self.flush_tail();
                while !data.is_empty() {
                    let chunk = data.split_to(data.len().min(STDIN_CHUNK_SIZE));
                    self.push_chunk(chunk);
                }
            }
        }
    }

    /// The merged stdin becomes a chunk, so that later stdin goes behind it.
    fn flush_tail(&mut self) {
        if !self.tail.is_empty() {
            let tail = self.take_tail();
            self.push_chunk(tail);
        }
    }

    /// The merged stdin in a buffer of its own. A part of the buffer of the
    /// tail would keep all of it, which can be far more than the stdin it
    /// counts. The tail keeps its buffer, of at most twice
    /// [`STDIN_CHUNK_SIZE`], for the next stdin.
    fn take_tail(&mut self) -> Bytes {
        let tail = Bytes::copy_from_slice(&self.tail);
        self.tail.clear();
        tail
    }

    fn push_chunk(&mut self, chunk: Bytes) {
        self.bytes += chunk.len() + STDIN_ENTRY_OVERHEAD;
        self.chunks.push_back(chunk);
    }

    /// The oldest stdin, callers check that there is some.
    pub(super) fn pop(&mut self) -> Stdin {
        if let Some(chunk) = self.chunks.pop_front() {
            self.bytes -= chunk.len() + STDIN_ENTRY_OVERHEAD;
            return Stdin::Data(chunk);
        }
        if !self.tail.is_empty() {
            return Stdin::Data(self.take_tail());
        }
        self.close_pending = false;
        Stdin::Close
    }

    /// Drop the stdin, nothing takes it anymore.
    pub(super) fn clear(&mut self) {
        self.chunks.clear();
        self.tail.clear();
        self.bytes = 0;
        self.close_pending = false;
    }
}
