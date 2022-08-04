use anyhow::{anyhow, Context, Result};
use lazy_static::lazy_static;
use nix::sys::statfs::{statfs, FsType};
use notify::{Error, Event, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use std::os::unix::prelude::AsRawFd;
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{channel, Receiver};
use tokio::task::{self, JoinHandle};
use tokio_eventfd::EventFd;
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, error, Instrument};

#[cfg(any(all(target_os = "linux", target_env = "musl")))]
pub const CGROUP2_SUPER_MAGIC: FsType = FsType(libc::CGROUP2_SUPER_MAGIC as u64);
#[cfg(any(all(target_os = "linux", target_arch = "s390x", not(target_env = "musl"))))]
pub const CGROUP2_SUPER_MAGIC: FsType = FsType(libc::CGROUP2_SUPER_MAGIC as u32);
#[cfg(any(all(target_os = "linux", target_arch = "armhfp", not(target_env = "musl"))))]
pub const CGROUP2_SUPER_MAGIC: FsType = FsType(libc::CGROUP2_SUPER_MAGIC as i32);
#[cfg(any(all(
    target_os = "linux",
    not(target_arch = "s390x"),
    not(target_arch = "armhfp"),
    not(target_env = "musl")
)))]
pub const CGROUP2_SUPER_MAGIC: FsType = FsType(libc::CGROUP2_SUPER_MAGIC as i64);

static CGROUP_ROOT: &str = "/sys/fs/cgroup";

lazy_static! {
    static ref IS_CGROUP_V2: bool = {
        if let Ok(sts) = statfs(CGROUP_ROOT) {
            return sts.filesystem_type() == CGROUP2_SUPER_MAGIC;
        }
        false
    };
}

pub struct OOMWatcher {
    pid: u32,
    token: CancellationToken,
    task: JoinHandle<()>,
}

#[derive(Debug)]
pub struct OOMEvent {
    pub oom: bool,
}

impl OOMWatcher {
    pub async fn new(
        token: &CancellationToken,
        pid: u32,
        exit_paths: &[PathBuf],
        tx: tokio::sync::mpsc::Sender<OOMEvent>,
    ) -> OOMWatcher {
        let exit_paths = exit_paths.to_owned();
        let token = token.clone();
        let task = {
            let stop = token.clone();
            task::spawn(
                async move {
                    if let Err(e) = if *IS_CGROUP_V2 {
                        Self::oom_handling_cgroup_v2(stop, pid, &exit_paths, tx)
                            .await
                            .context("setup cgroupv2 oom handling")
                    } else {
                        Self::oom_handling_cgroup_v1(stop, pid, &exit_paths, tx)
                            .await
                            .context("setup cgroupv1 oom handling")
                    } {
                        error!("Failed to watch OOM: {:#}", e)
                    }
                }
                .instrument(debug_span!("cgroup_handling")),
            )
        };
        OOMWatcher { pid, token, task }
    }

    pub async fn stop(self) {
        self.token.cancel();
        if let Err(e) = self.task.await {
            error!(pid = self.pid, "Stop failed: {:#}", e);
        }
    }

    async fn oom_handling_cgroup_v1(
        token: CancellationToken,
        pid: u32,
        exit_paths: &[PathBuf],
        tx: tokio::sync::mpsc::Sender<OOMEvent>,
    ) -> Result<()> {
        let span = debug_span!("oom_handling_cgroup_v1", pid);
        let _enter = span.enter();
        let memory_cgroup_path = Self::process_cgroup_subsystem_path(pid, false, "memory").await?;
        let memory_cgroup_file_oom_path = memory_cgroup_path.join("memory.oom_control");
        let event_control_path = memory_cgroup_path.join("cgroup.event_control");
        let path = memory_cgroup_file_oom_path.to_str();

        debug!(path, "Setup cgroup v1 oom handling");

        let oom_cgroup_file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(memory_cgroup_file_oom_path)
            .await
            .context("opening cgroup file")?;
        let mut oom_event_fd = EventFd::new(0, false).context("creating eventfd")?;

        let mut event_control = tokio::fs::OpenOptions::new()
            .write(true)
            .open(event_control_path)
            .await
            .context("opening cgroup file")?;
        event_control
            .write_all(
                format!(
                    "{} {}",
                    oom_event_fd.as_raw_fd(),
                    oom_cgroup_file.as_raw_fd()
                )
                .as_bytes(),
            )
            .await
            .context("writing control data")?;
        event_control.flush().await.context("flush control data")?;

        debug!("Successfully setup cgroup v1 oom detection");

        let mut buffer = [0u8; 16];
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    debug!("Loop cancelled");
                    let _ = tx.try_send(OOMEvent{ oom: false });
                    break;
                }
                _ = oom_event_fd.read(&mut buffer) => {
                    debug!("Got oom event");
                    if let Err(e) = Self::write_oom_files(exit_paths).await {
                        error!("Writing oom files failed: {:#}", e);
                    } else {
                        debug!("Successfully wrote oom files");
                    }
                    let _ = tx.try_send(OOMEvent{ oom: true });
                    break;
                }
            }
        }

        debug!("Done watching for ooms");
        Ok(())
    }

    fn async_watcher() -> Result<(RecommendedWatcher, Receiver<notify::Result<Event>>)> {
        let (tx, rx) = channel(1);

        let watcher = notify::recommended_watcher(move |res: Result<Event, Error>| {
            futures::executor::block_on(async {
                if let Err(e) = tx.send(res).await {
                    error!("Unable to send event result: {:#}", e)
                }
            })
        })?;

        Ok((watcher, rx))
    }

    async fn oom_handling_cgroup_v2(
        token: CancellationToken,
        pid: u32,
        exit_paths: &[PathBuf],
        tx: tokio::sync::mpsc::Sender<OOMEvent>,
    ) -> Result<()> {
        let span = debug_span!("oom_handling_cgroup_v2", pid);
        let _enter = span.enter();
        let subsystem_path = Self::process_cgroup_subsystem_path(pid, true, "memory").await?;
        let memory_events_file_path = subsystem_path.join("memory.events");
        let mut last_counter: u64 = 0;

        let path = memory_events_file_path.to_str();
        debug!(path, "Setup cgroup v2 handling");

        let (mut watcher, mut rx) = Self::async_watcher()?;
        watcher.watch(&memory_events_file_path, RecursiveMode::NonRecursive)?;

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    debug!("Loop cancelled");
                    match tx.try_send(OOMEvent{ oom: false }) {
                        Ok(_) => break,
                        Err(e) => error!("try_send failed: {:#}", e)
                    };
                    break;
                }
                Some(res) = rx.recv() => {
                    match res {
                        Ok(event) => {
                            if event.kind.is_remove() || event.kind.is_other() {
                                match tx.try_send(OOMEvent{ oom: false }) {
                                    Ok(_) => break,
                                    Err(e) => error!("try_send failed: {:#}", e)
                                };
                                break
                            }
                            if !event.kind.is_modify() {
                                continue;
                            }
                            debug!("Found modify event");
                            match Self::check_for_oom(&memory_events_file_path, last_counter).await {
                                Ok((counter, is_oom)) => {
                                    if !is_oom {
                                        continue;
                                    }
                                    debug!(counter, "Found oom event");
                                    if let Err(e) = Self::write_oom_files(exit_paths).await {
                                        error!("Writing oom files failed: {:#}", e);
                                    }
                                    last_counter = counter;
                                    match tx.try_send(OOMEvent{ oom: true }) {
                                        Ok(_) => break,
                                        Err(e) => error!("try_send failed: {:#}", e)
                                    };
                                }
                                Err(e) => {
                                    error!("Checking for oom failed: {}", e);
                                    match tx.try_send(OOMEvent{ oom: false }) {
                                        Ok(_) => break,
                                        Err(e) => error!("try_send failed: {:#}", e)
                                    };
                                }
                            };
                        },
                        Err(e) => {
                            debug!("Watch error: {:#}", e);
                            match tx.try_send(OOMEvent{ oom: false }) {
                                Ok(_) => break,
                                Err(e) => error!("try_send failed: {:#}", e)
                            };
                            break;
                        },
                    };
                }
            }
        }
        watcher.unwatch(&memory_events_file_path)?;

        debug!("Done watching for ooms");

        Ok(())
    }

    async fn check_for_oom(
        memory_events_file_path: &Path,
        last_counter: u64,
    ) -> Result<(u64, bool)> {
        let mut new_counter: u64 = 0;
        let mut found_oom = false;
        let fp = File::open(memory_events_file_path).await?;
        let reader = BufReader::new(fp);
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if let Some(counter) = line.strip_prefix("oom ") {
                let counter = counter.to_string().parse::<u64>()?;
                if counter != last_counter {
                    new_counter = counter;
                    found_oom = true;
                    break;
                }
            }
        }
        Ok((new_counter, found_oom))
    }

    async fn write_oom_files(exit_paths: &[PathBuf]) -> Result<()> {
        let paths = exit_paths.to_owned();
        let tasks: Vec<_> = paths
            .into_iter()
            .map(|path| {
                tokio::spawn(
                    async move {
                        debug!("Writing OOM file: {}", path.display());
                        if let Err(e) = File::create(&path).await {
                            error!("Could not write oom file to {}: {:#}", path.display(), e);
                        }
                    }
                    .instrument(debug_span!("write_oom_file")),
                )
            })
            .collect();
        for task in tasks {
            task.await?;
        }
        Ok(())
    }

    async fn process_cgroup_subsystem_path(
        pid: u32,
        is_cgroupv2: bool,
        subsystem: &str,
    ) -> Result<PathBuf> {
        if is_cgroupv2 {
            Self::process_cgroup_subsystem_path_cgroup_v2(pid).await
        } else {
            Self::process_cgroup_subsystem_path_cgroup_v1(pid, subsystem).await
        }
    }

    async fn process_cgroup_subsystem_path_cgroup_v1(pid: u32, subsystem: &str) -> Result<PathBuf> {
        lazy_static! {
            static ref RE: Regex = Regex::new(".*:(.*):/(.*)").expect("could not compile regex");
        }

        let cgroup_path = format!("/proc/{}/cgroup", pid);
        debug!("Using cgroup path: {}", cgroup_path);
        let fp = File::open(cgroup_path).await?;
        let reader = BufReader::new(fp);
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if let Some(caps) = RE.captures(&line) {
                let system = caps[1].to_string();
                let path = caps[2].to_string();
                if system.contains(subsystem) || system.eq("") {
                    return Ok(PathBuf::from(CGROUP_ROOT).join(subsystem).join(path));
                }
            }
        }
        Err(anyhow!("no path found"))
    }

    async fn process_cgroup_subsystem_path_cgroup_v2(pid: u32) -> Result<PathBuf> {
        lazy_static! {
            static ref RE: Regex = Regex::new(".*:.*:/(.*)").expect("could not compile regex");
        }

        let fp = File::open(format!("/proc/{}/cgroup", pid)).await?;
        let mut buffer = String::new();
        let mut reader = BufReader::new(fp);
        if reader.read_line(&mut buffer).await? == 0 {
            Err(anyhow!("invalid cgroup"))
        } else if let Some(caps) = RE.captures(&buffer) {
            Ok(Path::new(CGROUP_ROOT).join(&caps[1]))
        } else {
            Err(anyhow!("invalid cgroup"))
        }
    }
}
