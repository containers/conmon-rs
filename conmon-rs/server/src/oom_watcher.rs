//! Watching the cgroup of a container process for OOM kills, through the
//! memory events of the unified hierarchy of cgroup v2. A host with cgroup
//! v1, or with both in a hybrid setup, runs its containers without the
//! detection.

mod cgroup_path;
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) mod fuzzing;
mod inotify;
mod v2;

use anyhow::Context;
use nix::{
    sys::{
        signal::Signal,
        statfs::{CGROUP2_SUPER_MAGIC, statfs},
    },
    unistd::Pid,
};
use std::{
    num::ParseIntError,
    path::PathBuf,
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::fs::OpenOptions;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Capacity of the channel delivering file watch events to one OOM watcher.
const WATCH_EVENT_CHANNEL_SIZE: usize = 32;

static IS_CGROUP_V2: LazyLock<bool> = LazyLock::new(|| {
    statfs(CGROUP_ROOT).is_ok_and(|sts| sts.filesystem_type() == CGROUP2_SUPER_MAGIC)
});

/// Whether the server told that it does not watch for OOM kills, see
/// [`OOMWatcher::supported`].
static UNSUPPORTED_WARNED: AtomicBool = AtomicBool::new(false);

/// Watches the cgroup of a process for OOM kills, see
/// [`OOMWatcher::watch`].
pub(crate) struct OOMWatcher;

/// What watching a process for OOM kills found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OOMEvent {
    pub(crate) oom: bool,

    /// The cgroup of the process has no memory events, because it got
    /// removed or has no memory controller, which hides whether an OOM
    /// happened, but there is a hint which may also stem from a sibling: a
    /// new OOM kill of an outer cgroup, or none to tell. So it only counts
    /// for a process which got killed by SIGKILL, see [`OOMEvent::is_oom`].
    pub(crate) possible_oom: bool,
}

impl OOMEvent {
    fn new(oom: bool) -> Self {
        Self {
            oom,
            ..Self::default()
        }
    }

    /// A possible OOM if `possible`, none otherwise.
    pub(crate) fn possible(possible: bool) -> Self {
        Self {
            oom: false,
            possible_oom: possible,
        }
    }

    /// Whether the process which exited with `exit_code` got OOM killed.
    pub(crate) fn is_oom(self, exit_code: i32) -> bool {
        self.oom || (self.possible_oom && exit_code == 128 + Signal::SIGKILL as i32)
    }
}

impl OOMWatcher {
    /// Watch the cgroup of the process for an OOM until `token` gets
    /// cancelled, which the exit of the process does, and return what was
    /// found. The OOM files get written for an OOM found on its own. None if
    /// the cgroup could not be watched, like without cgroup v2, or the
    /// process was gone already.
    pub(crate) async fn watch(
        token: &CancellationToken,
        pid: Pid,
        exit_paths: &[PathBuf],
    ) -> Option<OOMEvent> {
        if !Self::supported(*IS_CGROUP_V2, &UNSUPPORTED_WARNED) {
            return None;
        }
        let event = Self::oom_handling_cgroup_v2(token, pid)
            .await
            .context("set up cgroup v2 OOM handling")
            .inspect_err(|e| error!("Unable to watch OOM: {e:#}"))
            .ok()
            .flatten();
        if event.is_some_and(|event| event.oom) {
            Self::write_oom_files(exit_paths).await;
        }
        debug!("Done watching for OOMs");
        event
    }

    /// Whether the OOM kills of the host can be watched, which takes the
    /// unified hierarchy of cgroup v2 at the cgroup root. Without it, like
    /// with cgroup v1 or a hybrid setup, the containers run without the OOM
    /// detection, which a warning tells once per server, as noted in
    /// `warned`, instead of with every container.
    fn supported(cgroup_v2: bool, warned: &AtomicBool) -> bool {
        if !cgroup_v2 && !warned.swap(true, Ordering::Relaxed) {
            warn!(
                "No cgroup v2 at {CGROUP_ROOT}, the OOM kills of containers do not get detected: cgroup v1 and hybrid setups are not supported"
            );
        }
        cgroup_v2
    }

    /// The counters of a cgroup file of `key value` lines, like
    /// `memory.events`, by their key.
    fn counters(content: &str) -> impl Iterator<Item = (&str, Result<u64, ParseIntError>)> {
        content
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(key, value)| (key, value.trim().parse()))
    }

    /// Create the empty OOM files, like conmon does. A failure is only
    /// logged, the exit gets reported either way, only without the OOM. A
    /// symlink at the path does not get followed, like for the log files.
    pub(crate) async fn write_oom_files(exit_paths: &[PathBuf]) {
        for path in exit_paths {
            debug!("Writing OOM file: {}", path.display());
            let created = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
                .await;
            if let Err(e) = created {
                error!("Unable to create OOM file {}: {e:#}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::Log;

    #[test]
    fn only_cgroup_v2_gets_watched() -> anyhow::Result<()> {
        let (log, _guard) = Log::capture();
        let warned = AtomicBool::new(false);
        assert!(OOMWatcher::supported(true, &warned));
        assert!(!warned.load(Ordering::Relaxed));

        // Every container asks, the first one warns.
        for _ in 0..3 {
            assert!(!OOMWatcher::supported(false, &warned));
        }
        assert!(warned.load(Ordering::Relaxed));
        assert!(OOMWatcher::supported(true, &warned));
        let log = log.contents()?;
        assert_eq!(log.matches("WARN").count(), 1, "{log}");
        assert_eq!(log.matches("No cgroup v2 at").count(), 1, "{log}");
        Ok(())
    }

    #[test]
    fn oom_event_needs_kill_for_ancestor_oom() {
        assert!(OOMEvent::new(true).is_oom(0));
        assert!(!OOMEvent::default().is_oom(137));
        let ancestor = OOMEvent {
            oom: false,
            possible_oom: true,
        };
        assert!(ancestor.is_oom(137));
        assert!(!ancestor.is_oom(143));
        assert!(!ancestor.is_oom(1));
    }

    #[tokio::test]
    async fn oom_files_do_not_follow_symlinks() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("target");
        std::fs::write(&target, "untouched")?;
        let link = dir.path().join("oom");
        std::os::unix::fs::symlink(&target, &link)?;
        let plain = dir.path().join("plain");

        // The failure for the symlink does not keep the other file from
        // getting created.
        OOMWatcher::write_oom_files(&[link.clone(), plain.clone()]).await;
        assert_eq!(std::fs::read_to_string(&target)?, "untouched");
        assert!(std::fs::symlink_metadata(&link)?.is_symlink());
        assert!(plain.is_file());
        Ok(())
    }
}
