use super::*;
use crate::mutex::lock;
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use tempfile::NamedTempFile;
use tokio::{
    sync::watch,
    task,
    time::{Duration, timeout},
};

/// How far the watch of a cgroup v2 leaf got, which the tests wait for
/// instead of sleeping.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Progress {
    /// How often its files got watched.
    pub(super) subscribed: usize,

    /// How many batches of events got handled.
    pub(super) handled: usize,

    /// The counters of the outer cgroup after the last batch.
    pub(super) outer: Option<OomCounters>,

    /// Whether it stopped watching and waits for the exit.
    pub(super) ended: bool,
}

static PROGRESS: Mutex<Vec<(PathBuf, Arc<watch::Sender<Progress>>)>> = Mutex::new(Vec::new());

/// The progress of the watch of a leaf, see [`Progress`].
pub(super) fn progress(leaf: &Path) -> Arc<watch::Sender<Progress>> {
    let mut all = lock(&PROGRESS);
    if let Some((_, progress)) = all.iter().find(|(path, _)| path == leaf) {
        return progress.clone();
    }
    let progress = Arc::new(watch::channel(Progress::default()).0);
    all.push((leaf.to_path_buf(), progress.clone()));
    progress
}

/// Wait until the watch of the leaf got as far as `f` tells.
async fn wait_progress(leaf: &Path, f: impl Fn(&Progress) -> bool) -> Result<()> {
    let mut rx = progress(leaf).subscribe();
    timeout(Duration::from_secs(5), rx.wait_for(f))
        .await
        .context("watch did not get as far")??;
    Ok(())
}

/// Wait until the watch of the leaf took the outer cgroup at `oom_kill`
/// kills as baseline.
async fn outer_kills_seen(leaf: &Path, oom_kill: u64) -> Result<()> {
    wait_progress(leaf, |p| p.outer.is_some_and(|c| c.oom_kill == oom_kill)).await
}

#[tokio::test]
async fn cgroup_v2_removal_by_cgroupfs_is_no_oom() -> Result<()> {
    let root = tempfile::tempdir()?;
    let [_, pod, scope, leaf] = fake_pod(root.path(), false)?;
    let (token, task) = watch_v2(&leaf);
    baselines_taken(&leaf).await?;

    // cgroupfs ends the watches of the files of a removed cgroup with
    // IN_IGNORED only, there is no event of the file itself.
    let watcher = SharedWatcher::get()?;
    let events = leaf.join("memory.events");
    watcher.remove_watch(&events)?;
    timeout(Duration::from_secs(5), async {
        while watcher.watches(&events) {
            task::yield_now().await;
        }
    })
    .await?;
    std::fs::remove_dir_all(&scope)?;
    assert!(!task.is_finished());

    // The pod is still watched, a kill there after the removal may have
    // been the one of the container.
    write_events(&pod, 0, 4)?;
    wait_progress(&leaf, |p| p.ended).await?;
    token.cancel();
    assert_eq!(
        timeout(Duration::from_secs(5), task).await??,
        OOMEvent::possible(true)
    );
    Ok(())
}

/// Stop the reader of the shared watcher of the leaf, once the watch task
/// watches it, and wait until the task watches it again on a new one.
async fn stop_watcher(leaf: &Path) -> Result<()> {
    baselines_taken(leaf).await?;
    let failed = SharedWatcher::get()?;
    failed.stop();
    wait_progress(leaf, |p| p.subscribed >= 2).await?;
    assert!(!Arc::ptr_eq(&SharedWatcher::get()?, &failed));
    Ok(())
}

#[tokio::test]
async fn cgroup_v2_watches_again_after_the_watcher_stopped() -> Result<()> {
    // An OOM of the container after the failure still gets found.
    let root = tempfile::tempdir()?;
    let [_, _, _, leaf] = fake_pod(root.path(), false)?;
    let (token, task) = watch_v2(&leaf);
    stop_watcher(&leaf).await?;
    write_events(&leaf, 1, 4)?;
    let event = timeout(Duration::from_secs(5), task).await??;
    assert_eq!(event, OOMEvent::new(true));
    token.cancel();

    // The kill of a sibling after the failure is no possible OOM of the
    // container, whose scope got removed.
    let root = tempfile::tempdir()?;
    let [_, pod, scope, leaf] = fake_pod(root.path(), false)?;
    let (token, task) = watch_v2(&leaf);
    stop_watcher(&leaf).await?;
    write_events(&pod, 0, 4)?;
    outer_kills_seen(&leaf, 4).await?;
    std::fs::remove_dir_all(&scope)?;
    token.cancel();
    let event = timeout(Duration::from_secs(5), task).await??;
    assert_eq!(event, OOMEvent::default());
    Ok(())
}

/// Watch a fake cgroup v2 leaf in a task of its own.
fn watch_v2(leaf: &Path) -> (CancellationToken, task::JoinHandle<OOMEvent>) {
    let token = CancellationToken::new();
    let (stop, leaf) = (token.clone(), leaf.to_path_buf());
    let task = task::spawn(async move { OOMWatcher::watch_cgroup_v2(&stop, &leaf).await });
    (token, task)
}

/// Let the watch task of `leaf` take its baselines and watch its files.
/// A task which starts late would take changes as its baselines
/// otherwise.
async fn baselines_taken(leaf: &Path) -> Result<()> {
    wait_progress(leaf, |p| p.subscribed >= 1).await
}

fn write_events(dir: &Path, oom: u64, oom_kill: u64) -> Result<()> {
    std::fs::write(
        dir.join("memory.events"),
        format!("low 0\nhigh 0\nmax 0\noom {oom}\noom_kill {oom_kill}\n"),
    )?;
    Ok(())
}

#[tokio::test]
async fn cgroup_v2_event_of_leaf_is_oom() -> Result<()> {
    let pod = tempfile::tempdir()?;
    let leaf = pod.path().join("ctr");
    std::fs::create_dir(&leaf)?;
    write_events(&leaf, 0, 0)?;
    let (_token, task) = watch_v2(&leaf);
    baselines_taken(&leaf).await?;

    write_events(&leaf, 1, 1)?;
    let event = timeout(Duration::from_secs(5), task).await??;
    assert_eq!(event, OOMEvent::new(true));
    Ok(())
}

#[tokio::test]
async fn cgroup_v2_oom_without_kill_is_no_oom() -> Result<()> {
    let pod = tempfile::tempdir()?;
    let leaf = pod.path().join("ctr");
    std::fs::create_dir(&leaf)?;
    write_events(&leaf, 0, 0)?;
    let (token, task) = watch_v2(&leaf);
    baselines_taken(&leaf).await?;

    // The limit got hit, reclaim freed memory before a kill.
    write_events(&leaf, 1, 0)?;
    token.cancel();
    assert_eq!(
        timeout(Duration::from_secs(5), task).await??,
        OOMEvent::default()
    );
    Ok(())
}

#[tokio::test]
async fn cgroup_v2_unchanged_leaf_is_no_oom() -> Result<()> {
    let pod = tempfile::tempdir()?;
    let leaf = pod.path().join("ctr");
    std::fs::create_dir_all(&leaf)?;
    std::fs::create_dir(pod.path().join("sibling"))?;
    write_events(&leaf, 0, 0)?;
    write_events(pod.path(), 0, 0)?;
    let (token, task) = watch_v2(&leaf);
    baselines_taken(&leaf).await?;

    // A kill of a sibling while the leaf exists does not count.
    write_events(pod.path(), 1, 1)?;
    token.cancel();
    assert_eq!(
        timeout(Duration::from_secs(5), task).await??,
        OOMEvent::default()
    );
    Ok(())
}

/// A pod cgroup of the systemd driver with a sibling container and the
/// scope of a container, which holds the leaf with crun. Returns the
/// QoS and pod cgroups, the scope and the leaf.
fn fake_pod(root: &Path, crun: bool) -> Result<[PathBuf; 4]> {
    let qos = root.join("kubepods-burstable.slice");
    let pod = qos.join("kubepods-burstable-pod12ab_34cd.slice");
    let scope = pod.join("crio-ctr.scope");
    let leaf = if crun {
        scope.join("container")
    } else {
        scope.clone()
    };
    std::fs::create_dir_all(&leaf)?;
    std::fs::create_dir(pod.join("crio-sibling.scope"))?;
    for dir in [&qos, &pod, &scope, &leaf] {
        write_events(dir, 0, 3)?;
    }
    Ok([qos, pod, scope, leaf])
}

/// Watch the leaf, let a sibling get killed before if `sibling_kill`,
/// remove the scope, let `after_removal` count kills and return what the
/// watch found on exit.
async fn removed_scope(
    crun: bool,
    sibling_kill: bool,
    after_removal: impl Fn(&Path, &Path) -> Result<()>,
) -> Result<OOMEvent> {
    let root = tempfile::tempdir()?;
    let [qos, pod, scope, leaf] = fake_pod(root.path(), crun)?;
    let (token, task) = watch_v2(&leaf);
    baselines_taken(&leaf).await?;

    // The watch notices it while the leaf exists.
    if sibling_kill {
        write_events(&pod, 0, 4)?;
        outer_kills_seen(&leaf, 4).await?;
    }
    std::fs::remove_dir_all(&scope)?;
    after_removal(&qos, &pod)?;
    // Waiting for the exit, not taken for an OOM on its own.
    wait_progress(&leaf, |p| p.ended).await?;
    assert!(!task.is_finished());

    token.cancel();
    Ok(timeout(Duration::from_secs(5), task).await??)
}

#[tokio::test]
async fn cgroup_v2_removed_leaf_falls_back_to_pod() -> Result<()> {
    let possible = OOMEvent {
        oom: false,
        possible_oom: true,
    };
    for crun in [false, true] {
        // A kill which the pod counted after the leaf went away.
        let event = removed_scope(crun, false, |_, pod| write_events(pod, 0, 4)).await?;
        assert_eq!(event, possible);
        assert!(event.is_oom(137));
        assert!(!event.is_oom(0));

        // Without one, it is no OOM.
        let event = removed_scope(crun, false, |_, _| Ok(())).await?;
        assert_eq!(event, OOMEvent::default());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cgroup_v2_sibling_kills_do_not_count() -> Result<()> {
    for crun in [false, true] {
        // A sibling got killed while the leaf existed.
        let event = removed_scope(crun, true, |_, _| Ok(())).await?;
        assert_eq!(event, OOMEvent::default());
    }
    Ok(())
}

#[test]
fn cgroup_v2_lost_events_refresh_the_outer_baseline() -> Result<()> {
    let root = tempfile::tempdir()?;
    let [_, pod, scope, leaf] = fake_pod(root.path(), false)?;
    let mut cgroup = CgroupV2::new(&leaf);

    // A sibling got killed while the kernel dropped the events.
    write_events(&pod, 0, 4)?;
    assert!(!cgroup.check(cgroup.is_outer_event(&WatchEvent::LOST)));

    // The removal of the leaf afterwards is no possible OOM then.
    std::fs::remove_dir_all(&scope)?;
    assert_eq!(cgroup.check_on_exit(), OOMEvent::default());
    Ok(())
}

#[tokio::test]
async fn cgroup_v2_is_not_compared_above_the_pod() -> Result<()> {
    // The QoS cgroup has other pods.
    let event = removed_scope(false, false, |qos, _| write_events(qos, 0, 4)).await?;
    assert_eq!(event, OOMEvent::default());

    // Without the pod cgroup, nothing tells.
    let event = removed_scope(false, false, |_, pod| Ok(std::fs::remove_dir_all(pod)?)).await?;
    assert_eq!(event, OOMEvent::possible(true));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cgroup_v2_removed_leaf_without_pod_falls_back_to_parent() -> Result<()> {
    // A cgroup parent not created by the kubelet.
    let run = async |sibling_kill: bool, kill: bool, remove_parent: bool| -> Result<OOMEvent> {
        let root = tempfile::tempdir()?;
        let parent = root.path().join("custom.slice");
        let leaf = parent.join("ctr.scope");
        std::fs::create_dir_all(&leaf)?;
        std::fs::create_dir(parent.join("sibling.scope"))?;
        write_events(&parent, 0, 3)?;
        write_events(&leaf, 0, 3)?;
        let (token, task) = watch_v2(&leaf);
        baselines_taken(&leaf).await?;

        if sibling_kill {
            write_events(&parent, 0, 4)?;
            outer_kills_seen(&leaf, 4).await?;
        }
        std::fs::remove_dir_all(&leaf)?;
        if kill {
            write_events(&parent, 0, 5)?;
        }
        if remove_parent {
            std::fs::remove_dir_all(&parent)?;
        }
        token.cancel();
        Ok(timeout(Duration::from_secs(5), task).await??)
    };

    assert_eq!(run(false, true, false).await?, OOMEvent::possible(true));
    assert_eq!(run(false, false, false).await?, OOMEvent::default());
    assert_eq!(run(true, false, false).await?, OOMEvent::default());
    assert_eq!(run(false, false, true).await?, OOMEvent::possible(true));
    Ok(())
}

#[test]
fn pod_cgroup_names() {
    for name in [
        "kubepods-burstable-pod12ab_34cd.slice",
        "kubepods-pod12ab_34cd.slice",
        "pod12ab-34cd",
    ] {
        assert!(CgroupV2::is_pod_cgroup_name(name), "{name}");
    }
    for name in [
        "kubepods.slice",
        "kubepods-burstable.slice",
        "crio-12ab.scope",
        "pod",
        "podman.slice",
        "libpod-12ab.scope",
    ] {
        assert!(!CgroupV2::is_pod_cgroup_name(name), "{name}");
    }
}

#[test]
fn read_oom_counters_separately() -> Result<()> {
    let mut file = NamedTempFile::new()?;
    file.write_all(b"low 0\nhigh 0\nmax 3\noom 1\noom_kill 0\noom_group_kill 0\n")?;
    file.flush()?;

    let counters = OOMWatcher::read_oom_counters(file.path())?;
    assert_eq!(
        counters,
        OomCounters {
            oom: 1,
            oom_kill: 0,
            has_kill: true,
        }
    );

    // Unchanged counters are no OOM, even though they differ from each other.
    assert!(!counters.increased_since(&counters));
    let counters = |oom, oom_kill| OomCounters {
        oom,
        oom_kill,
        has_kill: true,
    };
    assert!(counters(1, 1).increased_since(&counters(1, 0)));
    assert!(!counters(0, 0).increased_since(&counters(1, 0)));
    // An OOM which killed nothing is none.
    assert!(!counters(2, 0).increased_since(&counters(1, 0)));

    // Without the kill counter, the OOMs count.
    let mut file = NamedTempFile::new()?;
    file.write_all(b"low 0\nhigh 0\nmax 3\noom 1\n")?;
    file.flush()?;
    let old = OOMWatcher::read_oom_counters(file.path())?;
    assert!(!old.has_kill);
    let new = OomCounters { oom: 2, ..old };
    assert!(new.increased_since(&old));
    Ok(())
}
