use super::{PendingStdin, ResizeEvent, STDIN_CHUNK_SIZE, Stdin};
use crate::streaming_server::remotecommand::{ClientEvent, fuzzing::messages};
use bytes::Bytes;
use std::collections::VecDeque;

/// The messages of a WebSocket client for every framing, see
/// `remotecommand::fuzzing::messages`, with the resize events they carry
/// parsed like the session does. A size which got parsed reads the same
/// from the JSON Kubernetes sends.
pub fn client_messages(input: &[u8]) {
    messages(input, |event| {
        if let ClientEvent::Resize(data) = event
            && let Ok(size) = ResizeEvent::parse(&data)
        {
            let json = format!(r#"{{"Width":{},"Height":{}}}"#, size.width, size.height);
            assert_eq!(ResizeEvent::parse(json.as_bytes()).ok(), Some(size));
        }
    });
}

/// Push, pop and clear pending stdin by the input, one operation per
/// byte, and compare it with the stdin of a model, which is a plain
/// queue of its bytes. The bytes of stdin count up, so that the order
/// shows. Like for a session, nothing gets pushed while it is full.
pub fn pending_stdin(input: &[u8]) {
    let mut pending = PendingStdin::default();
    let (mut model, mut closed, mut close_pending) = (VecDeque::new(), false, false);
    let mut counter = 0u8;
    let mut input = input.iter().copied();
    while let Some(op) = input.next() {
        match op % 4 {
            0 | 1 if pending.is_full() => {}
            0 => {
                // Up to two chunks, also small stdin which gets merged.
                let mut next = || usize::from(input.next().unwrap_or_default());
                let len = ((next() << 16) | (next() << 8) | next()) % (2 * STDIN_CHUNK_SIZE + 1);
                let data: Vec<u8> = (0..len)
                    .map(|_| {
                        counter = counter.wrapping_add(1);
                        counter
                    })
                    .collect();
                if !closed {
                    model.extend(&data);
                }
                pending.push(Stdin::Data(Bytes::from(data)));
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
                    assert!(data.iter().all(|b| model.pop_front() == Some(*b)));
                }
                Stdin::Close => {
                    assert!(model.is_empty() && close_pending);
                    close_pending = false;
                }
            },
            2 => {}
            _ => {
                pending.clear();
                model.clear();
                close_pending = false;
            }
        }
        assert_eq!(pending.is_empty(), model.is_empty() && !close_pending);
        assert!(pending.len() >= model.len());
        // What nothing holds counts nothing.
        if pending.is_empty() {
            assert_eq!(pending.len(), 0);
        }
    }
}
