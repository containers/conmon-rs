use super::OOMWatcher;

/// Parse the cgroup files which the OOM watcher reads: the `memory.events`
/// of cgroup v2 and the `/proc/<pid>/cgroup` of a container process, whose
/// cgroup path the container chooses if it may create cgroups.
pub fn cgroup_files(data: &[u8]) {
    let content = String::from_utf8_lossy(data);
    let _ = OOMWatcher::parse_oom_counters(&content);

    let _ = OOMWatcher::unified_cgroup_path(data);
    for line in content.lines() {
        let _ = OOMWatcher::cgroup_v2_path(line);
    }
}
