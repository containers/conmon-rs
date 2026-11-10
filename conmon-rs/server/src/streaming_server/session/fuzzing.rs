use super::{
    PendingStdin, ResizeEvent, Stdin,
    pending_stdin::{STDIN_CHUNK_SIZE, STDIN_MERGE_SIZE},
};
use crate::streaming_server::remotecommand::{ClientEvent, fuzzing::messages};
use bytes::Bytes;
use std::sync::LazyLock;

/// The messages of a WebSocket client for every framing, see
/// `remotecommand::fuzzing::messages`, with the resize events they carry
/// parsed like the session does, see `resize_event`. Returns whether the
/// input gave events and all of its resize events got parsed.
pub fn client_messages(input: &[u8]) -> bool {
    let (mut events, mut parsed) = (0, true);
    messages(input, |event| {
        events += 1;
        if let ClientEvent::Resize(data) = event {
            parsed &= resize_event(&data);
        }
    });
    events > 0 && parsed
}

/// Parse a resize event like the session does, whether that succeeded. A
/// size which got parsed reads the same from the JSON Kubernetes sends.
pub(crate) fn resize_event(data: &[u8]) -> bool {
    let Ok(size) = ResizeEvent::parse(data) else {
        return false;
    };
    let json = format!(r#"{{"Width":{},"Height":{}}}"#, size.width, size.height);
    assert_eq!(ResizeEvent::parse(json.as_bytes()).ok(), Some(size));
    true
}

/// The stdin of [`pending_stdin`] counts up modulo a prime, so that its
/// order shows also across the parts it gets merged and split into, whose
/// sizes are powers of two.
const STDIN_PATTERN_PERIOD: usize = 251;

/// The longest stdin [`pending_stdin`] pushes, see [`stdin_len`].
const MAX_STDIN_LEN: usize = 2 * STDIN_CHUNK_SIZE + 127;

/// The stdin of [`pending_stdin`], from which every push and pop is a
/// slice.
static STDIN_PATTERN: LazyLock<Vec<u8>> = LazyLock::new(|| {
    (0..STDIN_PATTERN_PERIOD + MAX_STDIN_LEN)
        .map(|i| u8::try_from(i % STDIN_PATTERN_PERIOD).expect("below the period"))
        .collect()
});

/// The length of a push by a byte for its class and one for an offset in
/// it: small stdin, also empty, and stdin around the sizes where pending
/// stdin merges and splits it.
fn stdin_len(class: u8, offset: u8) -> usize {
    let start = match class % 4 {
        0 => 0,
        1 => STDIN_MERGE_SIZE - 128,
        2 => STDIN_CHUNK_SIZE - 128,
        _ => 2 * STDIN_CHUNK_SIZE - 128,
    };
    start + usize::from(offset)
}

/// The stdin which starts at the position of the stream, as it is pushed.
fn stdin_at(position: usize, len: usize) -> &'static [u8] {
    let start = position % STDIN_PATTERN_PERIOD;
    &STDIN_PATTERN[start..start + len]
}

/// Push, pop and clear pending stdin by the input, one operation per
/// byte, and compare it with a model, which is the range of the stream
/// which is still pending. Like for a session, nothing gets pushed while
/// it is full.
pub fn pending_stdin(input: &[u8]) {
    let mut pending = PendingStdin::default();
    // The stream positions of the oldest pending byte and after the newest.
    let (mut taken, mut pushed) = (0, 0);
    let (mut closed, mut close_pending) = (false, false);
    let mut input = input.iter().copied();
    while let Some(op) = input.next() {
        match op % 4 {
            0 | 1 if pending.is_full() => {}
            0 => {
                let mut next = || input.next().unwrap_or_default();
                let len = stdin_len(next(), next());
                let data = stdin_at(pushed, len);
                if !closed {
                    pushed += len;
                }
                pending.push(Stdin::Data(Bytes::from_static(data)));
            }
            1 => {
                if !closed {
                    closed = true;
                    close_pending = true;
                }
                pending.push(Stdin::Close);
            }
            2 if !pending.is_empty() => match pending.pop() {
                Stdin::Data(data) => {
                    assert!(!data.is_empty() && data.len() <= STDIN_CHUNK_SIZE);
                    assert!(taken + data.len() <= pushed);
                    assert!(data == stdin_at(taken, data.len()));
                    taken += data.len();
                }
                Stdin::Close => {
                    assert!(taken == pushed && close_pending);
                    close_pending = false;
                }
            },
            2 => {}
            _ => {
                pending.clear();
                taken = pushed;
                close_pending = false;
            }
        }
        let model = pushed - taken;
        assert_eq!(pending.is_empty(), model == 0 && !close_pending);
        assert!(pending.len() >= model);
        // What nothing holds counts nothing.
        if pending.is_empty() {
            assert_eq!(pending.len(), 0);
        }
    }
}
