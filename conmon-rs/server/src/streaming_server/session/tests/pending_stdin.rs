//! The stdin which waits for the container, and the events around it.

use super::*;
use crate::streaming_server::session::pending_stdin::{
    STDIN_CHUNK_SIZE, STDIN_ENTRY_OVERHEAD, STDIN_MERGE_SIZE,
};

#[test]
fn resize_events_bypass_pending_stdin() {
    let mut pending = PendingStdin::default();
    let (resize_tx, resize_rx) = watch::channel(None);

    let mut read = |event| StreamingServer::read_event(event, &mut pending, &resize_tx);
    let stdin = || ClientEvent::Stdin(Stdin::Data(Bytes::from_static(b"a")));
    assert_eq!(read(stdin()), ControlFlow::Continue(()));
    assert_eq!(
        read(ClientEvent::Resize(Bytes::from_static(b"1"))),
        ControlFlow::Continue(())
    );
    assert_eq!(
        read(ClientEvent::Resize(Bytes::from_static(b"2"))),
        ControlFlow::Continue(())
    );

    // Only the latest size is kept, the stdin waits on its own.
    assert_eq!(resize_rx.borrow().as_deref(), Some(&b"2"[..]));
    assert_eq!(pending.pop(), Stdin::Data(Bytes::from_static(b"a")));
    assert!(pending.is_empty());

    let close = StreamingServer::read_event(ClientEvent::Closed, &mut pending, &resize_tx);
    assert_eq!(close, ControlFlow::Break(EventEnd::Closed));
    let too_large = StreamingServer::read_event(ClientEvent::TooLarge, &mut pending, &resize_tx);
    assert_eq!(too_large, ControlFlow::Break(EventEnd::TooLarge));
}

#[test]
fn pending_stdin_is_bounded() {
    let mut pending = PendingStdin::default();
    let len = PENDING_STDIN_MAX_BYTES - 64 * STDIN_ENTRY_OVERHEAD;
    pending.push(Stdin::Data(Bytes::from(vec![0; len])));
    assert_eq!(
        pending.len(),
        PENDING_STDIN_MAX_BYTES - 48 * STDIN_ENTRY_OVERHEAD
    );
    assert!(!pending.is_full());
    pending.push(Stdin::Close);
    assert!(!pending.is_full());
    // Nothing takes stdin after the close, it does not count.
    pending.push(Stdin::Data(Bytes::from(vec![0; 48 * STDIN_ENTRY_OVERHEAD])));
    assert!(!pending.is_full());

    let mut taken = 0;
    while !pending.is_empty() {
        match pending.pop() {
            Stdin::Data(data) => {
                assert!(data.len() <= STDIN_CHUNK_SIZE);
                taken += data.len();
            }
            Stdin::Close => assert!(pending.is_empty()),
        }
    }
    assert_eq!(taken, len);
    assert_eq!(pending.len(), 0);

    pending.clear();
    assert!(pending.is_empty());
}

#[test]
fn pending_stdin_drops_what_carries_nothing() {
    let mut pending = PendingStdin::default();
    pending.push(Stdin::Data(Bytes::new()));
    assert!(pending.is_empty());

    // Only the first close counts, also without a container which takes
    // the stdin.
    for _ in 0..1000 {
        pending.push(Stdin::Close);
    }
    assert_eq!(pending.len(), STDIN_ENTRY_OVERHEAD);
    assert_eq!(pending.pop(), Stdin::Close);
    assert!(pending.is_empty());
    pending.push(Stdin::Close);
    assert!(pending.is_empty());
}

#[test]
#[expect(clippy::cast_possible_truncation, reason = "a wrapping byte pattern")]
fn small_stdin_gets_merged() {
    let mut pending = PendingStdin::default();
    for i in 0..2 * STDIN_CHUNK_SIZE {
        pending.push(Stdin::Data(Bytes::from(vec![i as u8])));
    }
    // Two full entries, not one per byte.
    assert_eq!(pending.len(), 2 * (STDIN_CHUNK_SIZE + STDIN_ENTRY_OVERHEAD));
    pending.push(Stdin::Close);

    let mut data = vec![];
    while let Stdin::Data(chunk) = pending.pop() {
        data.extend_from_slice(&chunk);
    }
    assert_eq!(
        data,
        (0..2 * STDIN_CHUNK_SIZE)
            .map(|i| i as u8)
            .collect::<Vec<_>>()
    );
    assert!(pending.is_empty());
}

#[test]
fn small_stdin_does_not_keep_the_buffer_of_the_tail() {
    let mut pending = PendingStdin::default();
    pending.push(Stdin::Data(Bytes::from(vec![0; STDIN_MERGE_SIZE - 1])));
    pending.push(Stdin::Data(Bytes::from(vec![0; STDIN_MERGE_SIZE - 1])));
    // More than the tail holds, the merged stdin becomes a chunk.
    pending.push(Stdin::Data(Bytes::from_static(b"abc")));
    let Stdin::Data(merged) = pending.pop() else {
        panic!("no data");
    };
    let Stdin::Data(small) = pending.pop() else {
        panic!("no data");
    };
    assert_eq!(merged.len(), 2 * (STDIN_MERGE_SIZE - 1));
    assert_eq!(small, Bytes::from_static(b"abc"));
    // The stdin owns all of its buffer, see `Bytes::try_into_mut`.
    for data in [merged, small] {
        let len = data.len();
        assert!(data.try_into_mut().is_ok_and(|data| data.capacity() == len));
    }
}

#[test]
fn large_stdin_gets_split_without_copying() {
    let mut pending = PendingStdin::default();
    pending.push(Stdin::Data(Bytes::from_static(b"a")));
    let data = Bytes::from(vec![1; 2 * STDIN_CHUNK_SIZE + 1]);
    pending.push(Stdin::Data(data.clone()));

    // The small stdin before it keeps its place.
    assert_eq!(pending.pop(), Stdin::Data(Bytes::from_static(b"a")));
    let mut offset = 0;
    for len in [STDIN_CHUNK_SIZE, STDIN_CHUNK_SIZE, 1] {
        let Stdin::Data(chunk) = pending.pop() else {
            panic!("no data");
        };
        assert_eq!(chunk.len(), len);
        assert_eq!(chunk.as_ptr(), data[offset..].as_ptr());
        offset += len;
    }
    assert!(pending.is_empty());
}
