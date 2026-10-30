//! A buffer which keeps only the end of what got written to it.

/// Keeps the last `MAX` bytes of what got pushed, like the end of the stderr
/// of a runtime, which holds its error.
#[derive(Debug, Default)]
pub struct Tail<const MAX: usize> {
    buf: Vec<u8>,
}

impl<const MAX: usize> Tail<MAX> {
    pub fn push(&mut self, data: &[u8]) {
        // Only the end of a large push is kept, it does not get copied whole.
        let data = &data[data.len().saturating_sub(MAX)..];
        // Dropping the front only once it doubled keeps it amortized.
        if self.buf.len() + data.len() > 2 * MAX {
            self.buf.drain(..self.buf.len() - (MAX - data.len()));
        }
        self.buf.extend_from_slice(data);
    }

    /// The last `MAX` bytes.
    pub fn end(&self) -> &[u8] {
        &self.buf[self.buf.len().saturating_sub(MAX)..]
    }

    /// The last `MAX` bytes, without copying them.
    pub fn into_end(mut self) -> Vec<u8> {
        let excess = self.buf.len().saturating_sub(MAX);
        self.buf.drain(..excess);
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_end() {
        let mut tail = Tail::<4>::default();
        tail.push(b"ab");
        assert_eq!(tail.end(), b"ab");
        for i in 0..100u8 {
            tail.push(&[i]);
            assert!(tail.buf.len() <= 8);
        }
        assert_eq!(tail.end(), [96, 97, 98, 99]);

        // A large push does not grow the buffer beyond its bound.
        tail.push(&(0..100).collect::<Vec<u8>>());
        assert!(tail.buf.capacity() <= 8);
        assert_eq!(tail.end(), [96, 97, 98, 99]);
        tail.push(&[1, 2]);
        assert_eq!(tail.end(), [98, 99, 1, 2]);
        assert_eq!(tail.into_end(), [98, 99, 1, 2]);
    }
}
