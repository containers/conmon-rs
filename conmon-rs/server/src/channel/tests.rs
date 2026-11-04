use super::*;
use std::time::Duration;
use tokio::{task, time};

/// Upper bound for tests which wait for something, it only hits if the
/// waited for thing never happens.
const WAIT: Duration = Duration::from_secs(10);

/// The output of a task, which must end within [`WAIT`].
async fn finished<T>(task: task::JoinHandle<T>) -> T {
    time::timeout(WAIT, task)
        .await
        .expect("task finished")
        .expect("task did not panic")
}

#[tokio::test]
async fn keeps_the_order() -> Result<(), RecvError> {
    let (tx, rx) = bounded(4);
    for i in 0..4 {
        tx.send(i).await.expect("send");
    }
    for i in 0..4 {
        assert_eq!(rx.recv().await?, i);
    }
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    Ok(())
}

#[tokio::test]
async fn send_waits_while_full() {
    let (tx, rx) = bounded(1);
    tx.send(1).await.expect("send");
    let send = task::spawn(async move { tx.send(2).await });
    task::yield_now().await;
    assert!(!send.is_finished());

    assert_eq!(rx.recv().await, Ok(1));
    finished(send).await.expect("send");
    assert_eq!(rx.try_recv(), Ok(2));
}

#[test]
fn force_send_drops_the_oldest() {
    let (tx, rx) = bounded(2);
    for i in 0..5 {
        tx.force_send(i).expect("send");
    }
    assert_eq!(tx.len(), 2);
    assert_eq!(rx.try_recv(), Ok(3));
    assert_eq!(rx.try_recv(), Ok(4));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn close_keeps_the_queued_messages() {
    let (tx, rx) = bounded(1);
    tx.send(1).await.expect("send");
    let waiting = task::spawn({
        let tx = tx.clone();
        async move { tx.send(2).await }
    });
    task::yield_now().await;

    rx.clone().close();
    assert!(rx.is_closed());
    assert_eq!(finished(waiting).await, Err(SendError(2)));
    assert_eq!(tx.force_send(3), Err(SendError(3)));
    assert_eq!(rx.recv().await, Ok(1));
    assert_eq!(rx.recv().await, Err(RecvError));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
}

#[tokio::test]
async fn recv_ends_without_senders() {
    let (tx, rx) = bounded(2);
    let second = tx.clone();
    tx.send(1).await.expect("send");
    let recv = task::spawn(async move { (rx.recv().await, rx.recv().await, rx) });
    drop(tx);
    task::yield_now().await;
    assert!(!recv.is_finished());

    drop(second);
    let (first, end, rx) = finished(recv).await;
    assert_eq!(first, Ok(1));
    assert_eq!(end, Err(RecvError));
    assert!(rx.is_closed());
    assert_eq!(rx.sender_count(), 0);
}

#[tokio::test]
async fn send_ends_without_receivers() {
    let (tx, rx) = bounded(1);
    tx.send(1).await.expect("send");
    let send = task::spawn(async move { tx.send(2).await });
    task::yield_now().await;

    let second = rx.clone();
    drop(rx);
    task::yield_now().await;
    assert!(!send.is_finished());

    drop(second);
    assert_eq!(finished(send).await, Err(SendError(2)));
}

#[tokio::test]
async fn clones_share_the_messages() {
    let (tx, rx) = bounded(4);
    let other = rx.clone();
    let first = task::spawn({
        let rx = rx.clone();
        async move { rx.recv().await }
    });
    let second = task::spawn(async move { other.recv().await });
    task::yield_now().await;

    tx.send(1).await.expect("send");
    tx.send(2).await.expect("send");
    let mut received = [
        finished(first).await.expect("recv"),
        finished(second).await.expect("recv"),
    ];
    received.sort_unstable();
    assert_eq!(received, [1, 2]);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn dropping_the_receivers_frees_the_queue() {
    let message = Arc::new(());
    let (tx, rx) = bounded(1);
    tx.force_send(message.clone()).expect("send");
    assert_eq!(Arc::strong_count(&message), 2);
    drop(rx);
    assert_eq!(Arc::strong_count(&message), 1);
    assert!(tx.force_send(message).is_err());
}

#[tokio::test]
async fn closed_waits_for_the_close() {
    let (tx, rx) = bounded(1);
    let closed = task::spawn({
        let tx = tx.clone();
        async move { tx.closed().await }
    });
    tx.send(1).await.expect("send");
    assert_eq!(rx.recv().await, Ok(1));
    task::yield_now().await;
    assert!(!closed.is_finished());

    rx.close();
    finished(closed).await;
}
