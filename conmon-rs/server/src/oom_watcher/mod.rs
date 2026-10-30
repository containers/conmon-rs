//! Watching the cgroup of a container process for OOM kills: through an
//! eventfd of the memory controller with cgroup v1, or by watching the memory
//! events of the unified hierarchy with cgroup v2.

mod cgroup_path;
mod inotify;
mod v1;
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
    path::{Path, PathBuf},
    sync::LazyLock,
};
use tokio::{fs::File, io::ErrorKind};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Capacity of the channel delivering file watch events to one OOM watcher.
const WATCH_EVENT_CHANNEL_SIZE: usize = 32;

static IS_CGROUP_V2: LazyLock<bool> = LazyLock::new(|| {
    statfs(CGROUP_ROOT).is_ok_and(|sts| sts.filesystem_type() == CGROUP2_SUPER_MAGIC)
});

pub struct OOMWatcher;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OOMEvent {
    pub oom: bool,

    /// The cgroup of the process has no memory events, because it got
    /// removed or has no memory controller, which hides whether an OOM
    /// happened, but there is a hint which may also stem from a sibling:
    /// with cgroup v1, an event before the removal, with cgroup v2, a new
    /// OOM kill of an outer cgroup or none to tell. So it only counts for a
    /// process which got killed by SIGKILL, see [`OOMEvent::is_oom`].
    pub possible_oom: bool,
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
    pub fn is_oom(self, exit_code: i32) -> bool {
        self.oom || (self.possible_oom && exit_code == 128 + Signal::SIGKILL as i32)
    }
}

impl OOMWatcher {
    /// Watch the cgroup of the process for an OOM until `token` gets
    /// cancelled, which the exit of the process does, and return what was
    /// found. The OOM files get written for an OOM found on its own. None if
    /// the cgroup could not be watched, or the process was gone already.
    pub async fn watch(
        token: &CancellationToken,
        pid: Pid,
        exit_paths: &[PathBuf],
    ) -> Option<OOMEvent> {
        let res = if *IS_CGROUP_V2 {
            Self::oom_handling_cgroup_v2(token, pid)
                .await
                .context("setup cgroupv2 oom handling")
        } else {
            Self::oom_handling_cgroup_v1(token, pid)
                .await
                .context("setup cgroupv1 oom handling")
        };
        let event = res
            .inspect_err(|e| error!("Failed to watch OOM: {e:#}"))
            .ok()
            .flatten();
        if event.is_some_and(|event| event.oom) {
            Self::write_oom_files(exit_paths).await;
        }
        debug!("Done watching for ooms");
        event
    }

    /// The counters of a cgroup file of `key value` lines, like
    /// `memory.events`, by their key.
    fn counters(content: &str) -> impl Iterator<Item = (&str, Result<u64, ParseIntError>)> {
        content
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(key, value)| (key, value.trim().parse()))
    }

    /// Checks if a file does not exist on disk.
    fn file_not_found(f: impl AsRef<Path>) -> bool {
        std::fs::metadata(f).is_err_and(|e| e.kind() == ErrorKind::NotFound)
    }

    /// Create the empty OOM files, like conmon does. A failure is only
    /// logged, the exit gets reported either way, only without the OOM.
    pub async fn write_oom_files(exit_paths: &[PathBuf]) {
        for path in exit_paths {
            debug!("Writing OOM file: {}", path.display());
            if let Err(e) = File::create(path).await {
                error!("Could not create oom file at {}: {:#}", path.display(), e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
