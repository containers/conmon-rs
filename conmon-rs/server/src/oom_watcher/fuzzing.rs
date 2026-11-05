use super::{OOMWatcher, v1::OomControl};

/// Parse the cgroup files which the OOM watcher reads: the
/// `memory.oom_control` of cgroup v1, the `memory.events` of cgroup v2 and
/// the `/proc/<pid>/cgroup` of a container process, whose cgroup path the
/// container chooses if it may create cgroups. Both counter parsers agree
/// on the kill counter of a file which both take.
pub fn cgroup_files(data: &[u8]) {
    let content = String::from_utf8_lossy(data);
    let control = OomControl::parse(&content);
    if let Ok(counters) = OOMWatcher::parse_oom_counters(&content) {
        assert_eq!(
            control.oom_kill,
            counters.has_kill.then_some(counters.oom_kill)
        );
    }

    let _ = OOMWatcher::unified_cgroup_path(data);
    for line in content.lines() {
        let _ = OOMWatcher::cgroup_v1_path(line, "memory");
        let _ = OOMWatcher::cgroup_v2_path(line);
    }
}
