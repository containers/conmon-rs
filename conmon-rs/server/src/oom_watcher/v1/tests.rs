use super::*;
use nix::sys::signal::Signal;
use std::path::PathBuf;
use tokio::{
    io::AsyncWriteExt,
    task,
    time::{sleep, timeout},
};

#[tokio::test]
async fn eventfd_counts_get_read() -> Result<()> {
    let mut event_fd = AsyncEventFd::new()?;
    event_fd.0.get_ref().write(2)?;
    let mut buffer = [0; 8];
    assert_eq!(event_fd.read(&mut buffer).await?, 8);
    assert_eq!(u64::from_ne_bytes(buffer), 2);

    // Nothing else to read until the next event.
    assert!(
        timeout(Duration::from_millis(50), event_fd.read(&mut buffer))
            .await
            .is_err()
    );
    let mut short = [0; 4];
    assert!(event_fd.read(&mut short).await.is_err());
    Ok(())
}

#[test]
fn parse_oom_control() {
    assert_eq!(
        OomControl::parse("oom_kill_disable 0\nunder_oom 0\noom_kill 3\n"),
        OomControl {
            kill_disable: false,
            under_oom: false,
            oom_kill: Some(3),
        }
    );
    // Kernels before 4.13 have no counter.
    assert_eq!(
        OomControl::parse("oom_kill_disable 1\nunder_oom 1\n"),
        OomControl {
            kill_disable: true,
            under_oom: true,
            oom_kill: None,
        }
    );
}

/// A fake memory cgroup v1 with its `memory.oom_control` and a pipe which
/// stands in for the eventfd.
struct FakeCgroupV1 {
    dir: tempfile::TempDir,
    event_tx: tokio::io::DuplexStream,
    event_rx: Option<tokio::io::DuplexStream>,
    token: CancellationToken,
}

impl FakeCgroupV1 {
    fn new(oom_control: &str) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("memory.oom_control"), oom_control)?;
        let (event_tx, event_rx) = tokio::io::duplex(64);
        Ok(Self {
            dir,
            event_tx,
            event_rx: Some(event_rx),
            token: CancellationToken::new(),
        })
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("memory.oom_control")
    }

    fn set(&self, oom_control: &str) -> Result<()> {
        Ok(std::fs::write(self.path(), oom_control)?)
    }

    /// Signal the eventfd `count` times since the last read.
    async fn signal(&mut self, count: u64) -> Result<()> {
        Ok(self.event_tx.write_all(&count.to_ne_bytes()).await?)
    }

    fn watch(&mut self, baseline: Option<u64>) -> task::JoinHandle<bool> {
        let watch = self.watch_event(baseline);
        task::spawn(async move { watch.await.is_ok_and(|event| event.oom) })
    }

    fn watch_event(&mut self, baseline: Option<u64>) -> task::JoinHandle<OOMEvent> {
        let event_rx = self.event_rx.take().expect("watched once");
        let token = self.token.clone();
        let path = self.path();
        task::spawn(
            async move { OOMWatcher::watch_cgroup_v1(&token, event_rx, &path, baseline).await },
        )
    }
}

const KILLS_0: &str = "oom_kill_disable 0\nunder_oom 0\noom_kill 0\n";

const KILLS_1: &str = "oom_kill_disable 0\nunder_oom 0\noom_kill 1\n";

#[tokio::test]
async fn cgroup_v1_event_with_kill_is_oom() -> Result<()> {
    let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
    let watch = cgroup.watch(Some(0));
    cgroup.set(KILLS_1)?;
    cgroup.signal(1).await?;
    assert!(timeout(Duration::from_secs(5), watch).await??);
    Ok(())
}

#[tokio::test]
async fn cgroup_v1_event_without_kill_is_no_oom() -> Result<()> {
    // An OOM of the pod cgroup notifies this one too, the victim was a
    // sibling.
    let mut cgroup = FakeCgroupV1::new(KILLS_1)?;
    let watch = cgroup.watch(Some(1));
    cgroup.signal(1).await?;
    cgroup.token.cancel();
    assert!(!timeout(Duration::from_secs(5), watch).await??);
    Ok(())
}

#[tokio::test]
async fn cgroup_v1_kill_found_on_exit() -> Result<()> {
    // Whether the event got handled before or not, the kill counts.
    let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
    let watch = cgroup.watch(Some(0));
    cgroup.signal(1).await?;
    cgroup.set(KILLS_1)?;
    cgroup.token.cancel();
    assert!(timeout(Duration::from_secs(5), watch).await??);
    Ok(())
}

#[tokio::test]
async fn cgroup_v1_removal_is_no_oom() -> Result<()> {
    let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
    let watch = cgroup.watch(Some(0));
    std::fs::remove_file(cgroup.path())?;
    cgroup.signal(1).await?;
    // No exit needed, the watch ends with the cgroup.
    assert!(!timeout(Duration::from_secs(5), watch).await??);

    // A notification which came before the removal may be one.
    let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
    let watch = cgroup.watch_event(Some(0));
    std::fs::remove_file(cgroup.path())?;
    cgroup.signal(2).await?;
    let event = timeout(Duration::from_secs(5), watch).await??;
    assert_eq!(event, OOMEvent::possible(true));
    Ok(())
}

#[tokio::test]
async fn cgroup_v1_event_without_counter_is_oom() -> Result<()> {
    let mut cgroup = FakeCgroupV1::new("oom_kill_disable 0\nunder_oom 1\n")?;
    let watch = cgroup.watch(None);
    cgroup.signal(1).await?;
    assert!(timeout(Duration::from_secs(5), watch).await??);
    Ok(())
}

#[tokio::test]
async fn cgroup_v1_broken_eventfd_checks_on_exit() -> Result<()> {
    let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
    let watch = cgroup.watch(Some(0));
    // A short read is no event.
    cgroup.event_tx.write_all(&[1]).await?;
    cgroup.event_tx.shutdown().await?;
    cgroup.set(KILLS_1)?;
    cgroup.token.cancel();
    assert!(timeout(Duration::from_secs(5), watch).await??);
    Ok(())
}

/// Longer than all reads which confirm an event.
const CONFIRM_TIME: Duration = Duration::from_secs(2);

#[tokio::test(start_paused = true)]
async fn cgroup_v1_kill_counted_after_event() -> Result<()> {
    // The kernel signals before it kills, the counter follows later.
    let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
    let watch = cgroup.watch(Some(0));
    cgroup.signal(1).await?;
    sleep(Duration::from_millis(30)).await;
    cgroup.set(KILLS_1)?;
    assert!(timeout(Duration::from_secs(5), watch).await??);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cgroup_v1_removed_while_confirming_needs_a_kill() -> Result<()> {
    // systemd removes the scope before the kill showed up. That may also
    // be an OOM of a sibling under the limit of the pod and a normal
    // exit, only the exit status tells.
    let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
    let watch = cgroup.watch_event(Some(0));
    cgroup.signal(1).await?;
    sleep(Duration::from_millis(30)).await;
    std::fs::remove_file(cgroup.path())?;
    let event = timeout(Duration::from_secs(5), watch).await??;
    assert_eq!(event, OOMEvent::possible(true));
    assert!(event.is_oom(128 + Signal::SIGKILL as i32));
    assert!(!event.is_oom(0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cgroup_v1_removal_after_unconfirmed_event_is_no_oom() -> Result<()> {
    // A sibling got killed and this container survived it, a later
    // normal stop removes the cgroup.
    let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
    let watch = cgroup.watch(Some(0));
    cgroup.signal(1).await?;
    sleep(CONFIRM_TIME).await;
    assert!(!watch.is_finished());

    // Gone before the exit check.
    std::fs::remove_file(cgroup.path())?;
    cgroup.token.cancel();
    assert!(!timeout(Duration::from_secs(5), watch).await??);

    // Also if the removal signals the eventfd first.
    let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
    let watch = cgroup.watch(Some(0));
    cgroup.signal(1).await?;
    sleep(CONFIRM_TIME).await;
    std::fs::remove_file(cgroup.path())?;
    cgroup.signal(1).await?;
    assert!(!timeout(Duration::from_secs(5), watch).await??);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cgroup_v1_unconfirmed_event_of_existing_cgroup_is_no_oom() -> Result<()> {
    // A sibling got killed, the cgroup stays and is not under OOM.
    let mut cgroup = FakeCgroupV1::new(KILLS_0)?;
    let watch = cgroup.watch(Some(0));
    cgroup.signal(1).await?;
    sleep(CONFIRM_TIME).await;
    cgroup.token.cancel();
    assert!(!timeout(Duration::from_secs(5), watch).await??);
    Ok(())
}

#[tokio::test]
async fn cgroup_v1_unknown_baseline_is_no_oom_on_exit() -> Result<()> {
    // Kills from before the container, without an event since.
    let mut cgroup = FakeCgroupV1::new("oom_kill_disable 0\nunder_oom 0\noom_kill 5\n")?;
    let watch = cgroup.watch(None);
    cgroup.token.cancel();
    assert!(!timeout(Duration::from_secs(5), watch).await??);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cgroup_v1_under_oom_is_oom() -> Result<()> {
    // The victim is in a nested memcg, which the counter misses.
    let mut cgroup = FakeCgroupV1::new("oom_kill_disable 0\nunder_oom 1\noom_kill 0\n")?;
    let watch = cgroup.watch(Some(0));
    cgroup.signal(1).await?;
    assert!(timeout(Duration::from_secs(5), watch).await??);

    // Without the OOM killer the tasks wait in the OOM, which counts
    // right away.
    let mut cgroup = FakeCgroupV1::new("oom_kill_disable 1\nunder_oom 1\noom_kill 0\n")?;
    let start = time::Instant::now();
    let watch = cgroup.watch(Some(0));
    cgroup.signal(1).await?;
    assert!(timeout(Duration::from_secs(5), watch).await??);
    assert!(start.elapsed() < OOMWatcher::CONFIRM_BACKOFF[0]);
    Ok(())
}

#[tokio::test]
async fn cgroup_v1_unknown_baseline_takes_event_as_oom() -> Result<()> {
    let mut cgroup = FakeCgroupV1::new(KILLS_1)?;
    let watch = cgroup.watch(None);
    cgroup.signal(1).await?;
    assert!(timeout(Duration::from_secs(5), watch).await??);
    Ok(())
}
