use super::*;
use crate::{oom_watcher::WATCH_EVENT_CHANNEL_SIZE, test_util::wait_for};
use nix::sys::inotify::InitFlags;
use std::io::Write;
use tempfile::NamedTempFile;
use tokio::{
    sync::mpsc::{Receiver, channel},
    time::{Duration, timeout},
};

async fn wait_for_event(rx: &mut Receiver<WatchEvent>) -> Result<WatchEvent> {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .context("no event within timeout")?
        .context("channel closed")
}

/// The number of subscribers of a path.
fn subscribers(watcher: &SharedWatcher, path: &Path) -> Result<usize> {
    Ok(lock(&watcher.watches)
        .0
        .values()
        .filter(|watch| *watch.path == *path)
        .map(|watch| watch.subscribers.len())
        .sum())
}

#[test]
fn full_channel_ends_with_a_lost_event() {
    let (tx, mut rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let event = WatchEvent {
        path: Some(Path::new("/memory.events").into()),
        removed: false,
    };
    for _ in 0..2 * WATCH_EVENT_CHANNEL_SIZE {
        Watches::deliver(&tx, &event);
    }
    let mut received = vec![];
    while let Ok(event) = rx.try_recv() {
        received.push(event);
    }
    assert_eq!(received.len(), WATCH_EVENT_CHANNEL_SIZE);
    assert_eq!(received.pop(), Some(WatchEvent::LOST));
    assert!(received.iter().all(|received| *received == event));

    // Once there is room again, events get delivered as they are.
    Watches::deliver(&tx, &event);
    assert_eq!(rx.try_recv().ok(), Some(event));
}

#[tokio::test]
async fn shared_watcher_dispatches_by_path() -> Result<()> {
    let mut first = NamedTempFile::new()?;
    let second = NamedTempFile::new()?;
    let watcher = SharedWatcher::get()?;

    let (tx1, mut rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let (tx2, mut rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let _sub1 = watcher.subscribe(first.path(), tx1)?;
    let _sub2 = watcher.subscribe(second.path(), tx2)?;

    first.write_all(b"oom 1\n")?;
    first.flush()?;

    let event = wait_for_event(&mut rx1).await?;
    assert_eq!(event.path.as_deref(), Some(first.path()));
    assert!(!event.removed);
    assert!(rx2.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn shared_watcher_reports_removals() -> Result<()> {
    let file = NamedTempFile::new()?;
    let watcher = SharedWatcher::get()?;
    let (tx, mut rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let _sub = watcher.subscribe(file.path(), tx)?;

    let path = file.path().to_path_buf();
    file.close()?;
    // The link count changes first.
    loop {
        let event = wait_for_event(&mut rx).await?;
        assert_eq!(event.path.as_deref(), Some(path.as_path()));
        if event.removed {
            break;
        }
    }
    // The kernel dropped the watch, the descriptor is forgotten.
    wait_for(|| lock(&watcher.watches).0.is_empty())
        .await
        .context("watch not forgotten")?;
    Ok(())
}

#[test]
fn watches_keep_events_of_recreated_files_apart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("memory.events");
    let inotify = Inotify::init(InitFlags::IN_CLOEXEC)?;
    std::fs::write(&path, "")?;
    let old = inotify.add_watch(&path, WATCH_FLAGS)?;
    std::fs::remove_file(&path)?;
    std::fs::write(&path, "")?;
    let new = inotify.add_watch(&path, WATCH_FLAGS)?;
    assert_ne!(old, new);

    let mut watches = Watches::default();
    let (old_tx, mut old_rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let (new_tx, mut new_rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
    for (wd, tx) in [(old, old_tx), (new, new_tx)] {
        watches.0.insert(
            wd,
            Watch {
                path: path.as_path().into(),
                subscribers: vec![(0, tx)],
            },
        );
    }

    // The removal of the old file comes after the new one got watched.
    for mask in [AddWatchFlags::IN_DELETE_SELF, AddWatchFlags::IN_IGNORED] {
        watches.dispatch(&InotifyEvent {
            wd: old,
            mask,
            cookie: 0,
            name: None,
        });
    }
    assert!(old_rx.try_recv()?.removed);
    assert!(new_rx.try_recv().is_err());
    assert!(!watches.0.contains_key(&old));
    assert!(watches.0.contains_key(&new));
    Ok(())
}

#[test]
fn lost_events_reach_every_subscriber() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let inotify = Inotify::init(InitFlags::IN_CLOEXEC)?;
    let mut watches = Watches::default();
    let mut receivers = vec![];
    for name in ["a", "b"] {
        let path = dir.path().join(name);
        std::fs::write(&path, "")?;
        let wd = inotify.add_watch(&path, WATCH_FLAGS)?;
        let (tx, rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
        watches.0.insert(
            wd,
            Watch {
                path: path.as_path().into(),
                subscribers: vec![(0, tx)],
            },
        );
        receivers.push((wd, rx));
    }

    // The kernel reports an overflow of its queue without a watch.
    watches.dispatch(&InotifyEvent {
        wd: receivers[0].0,
        mask: AddWatchFlags::IN_Q_OVERFLOW,
        cookie: 0,
        name: None,
    });
    for (_, rx) in &mut receivers {
        let event = rx.try_recv()?;
        assert_eq!(event.path, None);
        assert!(!event.removed);
    }
    assert_eq!(watches.0.len(), 2);
    Ok(())
}

#[tokio::test]
async fn shared_watcher_ends_watches_on_ignored() -> Result<()> {
    let file = NamedTempFile::new()?;
    let watcher = SharedWatcher::get()?;
    let (tx, mut rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let sub = watcher.subscribe(file.path(), tx)?;

    // cgroupfs only sends IN_IGNORED for the files of a removed cgroup,
    // like removing the watch does.
    watcher.inotify.get_ref().rm_watch(sub.wd)?;
    assert_eq!(timeout(Duration::from_secs(5), rx.recv()).await?, None);
    assert!(lock(&watcher.watches).0.is_empty());
    drop(sub);
    Ok(())
}

#[tokio::test]
async fn stopped_watchers_get_replaced() -> Result<()> {
    let file = NamedTempFile::new()?;
    let watcher = SharedWatcher::get()?;
    let (tx, mut rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let _sub = watcher.subscribe(file.path(), tx)?;

    SharedWatcher::stopped(&watcher.watches, &watcher.failed);

    // The watches got dropped with their senders, which ends the channels
    // of the subscribers.
    assert!(lock(&watcher.watches).0.is_empty());
    assert_eq!(timeout(Duration::from_secs(5), rx.recv()).await?, None);
    // The next subscription gets a watcher which reads its events.
    assert!(!Arc::ptr_eq(&SharedWatcher::get()?, &watcher));
    Ok(())
}

/// The way the server keeps its single watcher, which the other tests
/// take from a slot per runtime.
#[tokio::test]
async fn shared_watcher_slot() -> Result<()> {
    let slot = Slot::default();
    let first = SharedWatcher::get_in(&slot)?;
    assert!(Arc::ptr_eq(&first, &SharedWatcher::get_in(&slot)?));

    // One whose reader stopped gets replaced.
    SharedWatcher::stopped(&first.watches, &first.failed);
    let second = SharedWatcher::get_in(&slot)?;
    assert!(!Arc::ptr_eq(&first, &second));

    // It goes with its last user, the next one reads events again.
    let gone = Arc::downgrade(&second);
    drop((first, second));
    assert_eq!(gone.strong_count(), 0);
    let mut file = NamedTempFile::new()?;
    let (tx, mut rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let _sub = SharedWatcher::get_in(&slot)?.subscribe(file.path(), tx)?;
    file.write_all(b"oom 1\n")?;
    file.flush()?;
    wait_for_event(&mut rx).await?;
    Ok(())
}

#[tokio::test]
async fn shared_watcher_unsubscribes_on_drop() -> Result<()> {
    let file = NamedTempFile::new()?;
    let watcher = SharedWatcher::get()?;

    let (tx1, _rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let (tx2, _rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let sub1 = watcher.subscribe(file.path(), tx1)?;
    let sub2 = watcher.subscribe(file.path(), tx2)?;
    assert_eq!(subscribers(&watcher, file.path())?, 2);

    drop(sub1);
    assert_eq!(subscribers(&watcher, file.path())?, 1);

    drop(sub2);
    assert!(lock(&watcher.watches).0.is_empty());

    Ok(())
}

#[tokio::test]
async fn shared_watchers_go_with_their_last_subscription() -> Result<()> {
    let file = NamedTempFile::new()?;
    let (tx, _rx) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let sub = SharedWatcher::get()?.subscribe(file.path(), tx)?;
    let watcher = Arc::downgrade(&sub.watcher);
    assert!(Arc::ptr_eq(&SharedWatcher::get()?, &sub.watcher));
    drop(sub);
    assert_eq!(watcher.strong_count(), 0);
    Ok(())
}

#[tokio::test]
async fn shared_watcher_resubscribe_same_path() -> Result<()> {
    let mut file = NamedTempFile::new()?;
    let watcher = SharedWatcher::get()?;

    let (tx1, _rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let _sub1 = watcher.subscribe(file.path(), tx1)?;

    // A second subscription re-adds the watch and still gets events.
    let (tx2, mut rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let _sub2 = watcher.subscribe(file.path(), tx2)?;

    file.write_all(b"oom 1\n")?;
    file.flush()?;

    wait_for_event(&mut rx2).await?;

    Ok(())
}

#[tokio::test]
async fn shared_watcher_keeps_watch_after_other_subscriber_left() -> Result<()> {
    let mut file = NamedTempFile::new()?;
    let watcher = SharedWatcher::get()?;

    let (tx1, mut rx1) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let _sub1 = watcher.subscribe(file.path(), tx1)?;

    // A second subscriber comes and goes, like an exec in the container.
    let (tx2, _rx2) = channel(WATCH_EVENT_CHANNEL_SIZE);
    let sub2 = watcher.subscribe(file.path(), tx2)?;
    drop(sub2);
    assert_eq!(subscribers(&watcher, file.path())?, 1);

    file.write_all(b"oom 1\n")?;
    file.flush()?;

    wait_for_event(&mut rx1).await?;

    Ok(())
}
