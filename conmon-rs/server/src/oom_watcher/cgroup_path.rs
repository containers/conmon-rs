//! Where the cgroup of a process is.

use super::{CGROUP_ROOT, OOMWatcher};
use anyhow::{Context, Result, bail};
use nix::unistd::Pid;
use std::{
    fs::File as StdFile,
    io::{BufRead, BufReader, ErrorKind},
    path::{Path, PathBuf},
};
use tracing::{debug, trace};

impl OOMWatcher {
    pub(super) fn process_cgroup_subsystem_path_cgroup_v1(
        pid: Pid,
        subsystem: &str,
    ) -> Result<Option<PathBuf>> {
        if let Some(fp) = Self::try_open_cgroup_path(pid)? {
            for line in BufReader::new(fp).lines() {
                let line = line.context("read line from cgroup file")?;
                if let Some(path) = Self::cgroup_v1_path(&line, subsystem)? {
                    return Ok(path.into());
                }
            }

            bail!("no path found")
        }

        Ok(None)
    }

    pub(super) fn process_cgroup_subsystem_path_cgroup_v2(pid: Pid) -> Result<Option<PathBuf>> {
        if let Some(fp) = Self::try_open_cgroup_path(pid)? {
            return Self::unified_cgroup_path(BufReader::new(fp)).map(Some);
        }

        Ok(None)
    }

    /// The cgroupfs path of the unified hierarchy from a `/proc/<pid>/cgroup`
    /// file. Named cgroup v1 hierarchies, like the one of systemd, are
    /// listed as well and may come first.
    fn unified_cgroup_path(cgroup_file: impl BufRead) -> Result<PathBuf> {
        for line in cgroup_file.lines() {
            let line = line.context("read line from cgroup file")?;
            if line.starts_with("0::") {
                return Self::cgroup_v2_path(&line);
            }
        }
        bail!("no unified hierarchy found in cgroup file")
    }

    /// The cgroupfs path of `subsystem` if the `/proc/<pid>/cgroup` line
    /// `hierarchy-ID:controller-list:cgroup-path` is the one of its hierarchy.
    /// The path may contain colons itself. The unified hierarchy of a hybrid
    /// setup lists no controllers, it is not the one of any subsystem.
    fn cgroup_v1_path(line: &str, subsystem: &str) -> Result<Option<PathBuf>> {
        let mut iter = line.splitn(3, ':').skip(1);
        let controllers = iter.next().context("no system found in cgroup")?;
        let path = Self::strip_cgroup_root(iter.next())?;

        if controllers.split(',').any(|c| c == subsystem) {
            return Ok(Some(Path::new(CGROUP_ROOT).join(subsystem).join(path)));
        }
        Ok(None)
    }

    /// The cgroupfs path of the unified hierarchy line `0::cgroup-path` of
    /// `/proc/<pid>/cgroup`.
    fn cgroup_v2_path(line: &str) -> Result<PathBuf> {
        let path = Self::strip_cgroup_root(line.splitn(3, ':').nth(2))?;
        Ok(Path::new(CGROUP_ROOT).join(path))
    }

    /// The cgroup path relative to the root of its hierarchy.
    fn strip_cgroup_root(path: Option<&str>) -> Result<&str> {
        Ok(path
            .context("no path found in cgroup")?
            .strip_prefix("/")
            .context("strip root path prefix")?
            .trim_end())
    }

    fn try_open_cgroup_path(pid: Pid) -> Result<Option<StdFile>> {
        let cgroup_path = PathBuf::from("/proc").join(pid.to_string()).join("cgroup");
        debug!("Using cgroup path: {}", cgroup_path.display());

        match StdFile::open(&cgroup_path) {
            Ok(file) => Ok(file.into()),
            // Short lived processes will not be handled as an error
            Err(error) if error.kind() == ErrorKind::NotFound => {
                trace!("Cgroup path not found: {}", cgroup_path.display());
                Ok(None)
            }
            Err(error) => bail!("open cgroup path {}: {}", cgroup_path.display(), error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_paths_from_proc_lines() -> Result<()> {
        let root = Path::new(CGROUP_ROOT);
        assert_eq!(
            OOMWatcher::cgroup_v1_path("4:memory:/kubepods/pod/ctr\n", "memory")?,
            Some(root.join("memory/kubepods/pod/ctr"))
        );
        assert_eq!(
            OOMWatcher::cgroup_v1_path("3:cpuset,memory:/a:b", "memory")?,
            Some(root.join("memory/a:b"))
        );
        // A controller only matches as a whole.
        assert_eq!(
            OOMWatcher::cgroup_v1_path("7:memory_extra:/x", "memory")?,
            None
        );
        assert_eq!(
            OOMWatcher::cgroup_v1_path("5:cpu,cpuacct:/x", "memory")?,
            None
        );
        // Neither the unified hierarchy of a hybrid setup.
        assert_eq!(OOMWatcher::cgroup_v1_path("0::/x", "memory")?, None);
        assert!(OOMWatcher::cgroup_v1_path("5:memory", "memory").is_err());
        assert!(OOMWatcher::cgroup_v1_path("5:memory:relative", "memory").is_err());

        assert_eq!(
            OOMWatcher::cgroup_v2_path("0::/kubepods.slice/crio-1:2.scope\n")?,
            root.join("kubepods.slice/crio-1:2.scope")
        );
        assert_eq!(OOMWatcher::cgroup_v2_path("0::/")?, root.to_path_buf());
        assert!(OOMWatcher::cgroup_v2_path("0:").is_err());

        // A named v1 hierarchy may come before the unified one.
        assert_eq!(
            OOMWatcher::unified_cgroup_path(
                &b"1:name=systemd:/user.slice\n0::/kubepods.slice/ctr\n"[..]
            )?,
            root.join("kubepods.slice/ctr")
        );
        assert_eq!(
            OOMWatcher::unified_cgroup_path(&b"0::/ctr\n"[..])?,
            root.join("ctr")
        );
        assert!(OOMWatcher::unified_cgroup_path(&b"1:name=systemd:/x\n"[..]).is_err());
        Ok(())
    }
}
