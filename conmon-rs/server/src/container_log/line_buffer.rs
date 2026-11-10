//! Collecting the output into whole lines, see [`LineBuffer`].

use super::{Line, Text, strip_line_ending};
use crate::container_io::{Pipe, READ_BUFFER_SIZE};
use bytes::BytesMut;

/// Collects the output of both pipes into whole lines for journald, which
/// writes one entry per line.
///
/// A read chunk may end anywhere, in the middle of a line, between "\r" and
/// "\n" or inside a multi byte character. Keeping the incomplete tail until
/// the next chunk of the same pipe arrives avoids split lines and garbled
/// characters. A line which grows beyond [`LineBuffer::MAX_LINE`] is written
/// as a partial one, cut at a character boundary, and so is a line which
/// waits too long for its ending, see [`LineBuffer::flush`].
#[derive(Debug, Default)]
pub(super) struct LineBuffer {
    pub(super) stdout: BytesMut,
    stderr: BytesMut,
}

impl LineBuffer {
    /// Longer lines get written as partial entries.
    pub(super) const MAX_LINE: usize = 16 * 1024;

    fn tail(&mut self, pipe: Pipe) -> &mut BytesMut {
        match pipe {
            Pipe::StdOut => &mut self.stdout,
            Pipe::StdErr => &mut self.stderr,
        }
    }

    /// Whether a line of the pipe waits for its ending.
    pub(super) fn has_pending(&self, pipe: Pipe) -> bool {
        !match pipe {
            Pipe::StdOut => &self.stdout,
            Pipe::StdErr => &self.stderr,
        }
        .is_empty()
    }

    /// Add a chunk and return the lines it completes. The whole lines of the
    /// chunk borrow it, only the end of a line which started in an earlier
    /// chunk and the start of one which continues in a later one get copied
    /// into the buffer of the pipe.
    pub(super) fn push<'a>(&mut self, pipe: Pipe, bytes: &'a [u8]) -> Vec<Line<'a>> {
        let mut lines = vec![];
        let mut ends = memchr::memchr_iter(b'\n', bytes).map(|end| end + 1);
        let mut start = 0;
        let tail = self.tail(pipe);
        if !tail.is_empty() {
            let end = ends.next();
            start = end.unwrap_or(bytes.len());
            tail.extend_from_slice(&bytes[..start]);
            if end.is_some() {
                let line = tail.split().freeze();
                lines.push(Line {
                    text: Text::Buffered(line.slice(..strip_line_ending(&line).len())),
                    partial: false,
                });
            }
        }

        for end in ends {
            lines.push(Line {
                text: Text::Chunk(strip_line_ending(&bytes[start..end])),
                partial: false,
            });
            start = end;
        }
        tail.extend_from_slice(&bytes[start..]);
        lines.extend(Self::cut_long(tail));
        lines
    }

    /// Cut a line which grew too long without its ending, see
    /// [`LineBuffer::cut`], and release the buffer once empty.
    fn cut_long(tail: &mut BytesMut) -> Option<Line<'static>> {
        let line = (tail.len() >= Self::MAX_LINE).then(|| Self::cut(tail));
        Self::shrink(tail);
        line
    }

    /// Cut the tail at a point which keeps an unfinished multi byte character
    /// and a "\r" waiting for its "\n" in the buffer. Invalid bytes are not
    /// held back, they would stay in front forever.
    fn cut(tail: &mut BytesMut) -> Line<'static> {
        let mut end = Self::complete_len(tail);
        if tail[..end].ends_with(b"\r") {
            end -= 1;
        }
        Line {
            text: Text::Buffered(tail.split_to(end).freeze()),
            partial: true,
        }
    }

    /// The length of the bytes without the unfinished multi byte character
    /// they end with, if any. Only the last character matters, which starts
    /// within the last three bytes if it is unfinished, an invalid byte in
    /// front of it does not.
    fn complete_len(bytes: &[u8]) -> usize {
        let window = bytes.len().saturating_sub(3);
        let Some(start) = bytes[window..]
            .iter()
            .rposition(|b| b & 0xc0 != 0x80)
            .map(|i| window + i)
        else {
            return bytes.len();
        };
        match str::from_utf8(&bytes[start..]) {
            Err(e) if e.error_len().is_none() => start + e.valid_up_to(),
            _ => bytes.len(),
        }
    }

    /// A line longer than a read chunk leaves a large buffer behind, release
    /// it once drained. An idle container keeps at most a chunk of it.
    fn shrink(tail: &mut BytesMut) {
        if tail.is_empty() && tail.capacity() > READ_BUFFER_SIZE {
            *tail = BytesMut::new();
        }
    }

    /// Return the line which waits for its ending as a partial one. The
    /// output paused, a prompt or progress output without a newline would
    /// otherwise stay invisible.
    pub(super) fn flush(&mut self, pipe: Pipe) -> Vec<Line<'static>> {
        // Like a long line, it keeps an unfinished multi byte character and
        // a "\r" back for the next chunk, see [`LineBuffer::cut`].
        let tail = self.tail(pipe);
        if tail.is_empty() {
            return vec![];
        }
        let line = Self::cut(tail);
        Self::shrink(tail);
        if line.text.is_empty() {
            return vec![];
        }
        vec![line]
    }

    /// The output of the pipe ended, return the line which still waits for
    /// its ending as a partial one.
    pub(super) fn finish(&mut self, pipe: Pipe) -> Vec<Line<'static>> {
        let tail = self.tail(pipe);
        if tail.is_empty() {
            return vec![];
        }
        // Without a newline, so there is no line ending to strip.
        let text = Text::Buffered(tail.split().freeze());
        Self::shrink(tail);
        vec![Line {
            text,
            partial: true,
        }]
    }
}

#[cfg(test)]
mod tests;
