use anyhow::{Context, Result, bail};
use lazy_static::lazy_static;
use nix::sys::statfs::{FsType, statfs};
use notify::{
    Error, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode},
};
use regex::Regex;
use std::{
    os::unix::prelude::AsRawFd,
    path::{Path, PathBuf},
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ErrorKind},
    sync::mpsc::{Receiver, Sender, channel},
    task,
};
use tokio_eventfd::EventFd;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, trace};

#[cfg(all(target_os = "linux", target_env = "musl"))]
pub const CGROUP2_SUPER_MAGIC: FsType = FsType(libc::CGROUP2_SUPER_MAGIC as u64);
#[cfg(all(target_os = "linux", target_arch = "s390x", not(target_env = "musl")))]
pub const CGROUP2_SUPER_MAGIC: FsType = FsType(libc::CGROUP2_SUPER_MAGIC as u32);
#[cfg(all(
    target_os = "linux",
    any(
        all(target_arch = "arm", not(target_env = "musl")),
        target_arch = "x86",
    )
))]
pub const CGROUP2_SUPER_MAGIC: FsType = FsType(libc::CGROUP2_SUPER_MAGIC as i32);
#[cfg(all(
    target_os = "linux",
    not(target_arch = "s390x"),
    not(target_arch = "arm"),
    not(target_arch = "x86"),
    not(target_env = "musl")
))]
pub const CGROUP2_SUPER_MAGIC: FsType = FsType(libc::CGROUP2_SUPER_MAGIC);

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
    token: CancellationToken,
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
        tx: Sender<OOMEvent>,
    ) -> OOMWatcher {
        let exit_paths = exit_paths.to_owned();
        let token = token.clone();
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
                    error!("Failed to watch OOM: {:#}", e);
                }
            }
            .instrument(debug_span!("cgroup_handling")),
        );
        OOMWatcher { token }
    }

    pub async fn stop(self) {
        self.token.cancel();
    }

    async fn oom_handling_cgroup_v1(
        token: CancellationToken,
        pid: u32,
        exit_paths: &[PathBuf],
        tx: Sender<OOMEvent>,
    ) -> Result<()> {
        let span = debug_span!("oom_handling_cgroup_v1", pid);
        let _enter = span.enter();

        let memory_cgroup_path = if let Some(path) =
            Self::process_cgroup_subsystem_path_cgroup_v1(pid, "memory")
                .await
                .context("process cgroup memory subsystem path")?
        {
            path
        } else {
            debug!("Stopping OOM handler because no cgroup subsystem path exists");
            return Ok(());
        };

        let memory_cgroup_file_oom_path = memory_cgroup_path.join("memory.oom_control");
        let event_control_path = memory_cgroup_path.join("cgroup.event_control");
        let path = memory_cgroup_file_oom_path.to_str();

        debug!(path, "Setup cgroup v1 oom handling");

        let oom_cgroup_file = OpenOptions::new()
            .write(true)
            .open(memory_cgroup_file_oom_path)
            .await
            .context("opening cgroup oom file")?;
        let mut oom_event_fd = EventFd::new(0, false).context("creating eventfd")?;

        let mut event_control = OpenOptions::new()
            .write(true)
            .open(event_control_path)
            .await
            .context("opening cgroup event control file")?;
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
        tokio::select! {
            _ = token.cancelled() => {
                debug!("Loop cancelled");
                let _ = tx.try_send(OOMEvent{ oom: false });
            }
            _ = oom_event_fd.read(&mut buffer) => {
                debug!("Got oom event");
                match Self::write_oom_files(exit_paths).await { Err(e) => {
                    error!("Writing oom files failed: {:#}", e);
                } _ => {
                    debug!("Successfully wrote oom files");
                }}
                let _ = tx.try_send(OOMEvent{ oom: true });
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
        })
        .context("get recommended watcher")?;

        Ok((watcher, rx))
    }

    async fn oom_handling_cgroup_v2(
        token: CancellationToken,
        pid: u32,
        exit_paths: &[PathBuf],
        tx: Sender<OOMEvent>,
    ) -> Result<()> {
        let span = debug_span!("oom_handling_cgroup_v2", pid);
        let _enter = span.enter();

        let subsystem_path = if let Some(path) = Self::process_cgroup_subsystem_path_cgroup_v2(pid)
            .await
            .context("process cgroup subsystem path")?
        {
            path
        } else {
            debug!("Stopping OOM handler because no cgroup subsystem path exists");
            return Ok(());
        };

        let memory_events_file_path = subsystem_path.join("memory.events");
        let mut last_counter: u64 = 0;

        let path = memory_events_file_path.to_str();
        debug!(path, "Setup cgroup v2 handling");

        let (mut watcher, mut rx) = Self::async_watcher().context("get async watcher")?;
        watcher
            .watch(&memory_events_file_path, RecursiveMode::NonRecursive)
            .context("watch memory events file")?;

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    debug!("Loop cancelled");
                    if let Err(e) = tx.try_send(OOMEvent{ oom: false }) {
                        error!("try_send failed: {:#}", e);
                    };

                    debug!("Last resort check for OOM");
                    match Self::check_for_oom(&memory_events_file_path, last_counter).await {
                        Ok((counter, is_oom)) => {
                            if !is_oom {
                                debug!("No OOM found in event");
                                break;
                            }
                            debug!("Found OOM event count {counter}");
                            if let Err(e) = Self::write_oom_files(exit_paths).await {
                                error!("Writing OOM files failed: {:#}", e)
                            }
                            if let Err(e) = tx.try_send(OOMEvent{ oom: true }) {
                                error!("Try send failed: {:#}", e)
                            };
                        }
                        // It is still possible to miss an OOM event here if the memory events file
                        // got removed between the notify event below and the token cancellation.
                        Err(e) => {
                            debug!("Checking for last resort OOM failed: {:#}", e);
                            break;
                        }
                    }

                    break;
                }
                Some(res) = rx.recv() => {
                    match res {
                        Ok(event) => {
                            // Skip access events since they're not if any interest
                            if event.kind == EventKind::Access(AccessKind::Open(AccessMode::Any)) {
                                continue;
                            }
                            debug!("Got OOM file event: {:?}", event);
                            if event.kind.is_remove() {
                                debug!("Got remove event");
                                if let Err(e) = tx.try_send(OOMEvent{ oom: false }) {
                                    error!("try_send failed: {:#}", e);
                                };
                                break
                            }
                            match Self::check_for_oom(&memory_events_file_path, last_counter).await {
                                Ok((counter, is_oom)) => {
                                    if !is_oom {
                                        debug!("No OOM found in event");
                                        continue;
                                    }
                                    debug!("Found OOM event count {}", counter);
                                    if let Err(e) = Self::write_oom_files(exit_paths).await {
                                        error!("Writing OOM files failed: {:#}", e);
                                    }
                                    last_counter = counter;
                                    match tx.try_send(OOMEvent{ oom: true }) {
                                        Ok(_) => break,
                                        Err(e) => error!("try_send failed: {:#}", e)
                                    };
                                }
                                Err(e) => {
                                    error!("Checking for OOM failed: {:#}", e);

                                    let mut oom = false;
                                    if Self::file_not_found(&memory_events_file_path).await {
                                        debug!("Assuming memory slice removal race, still reporting one OOM event");
                                        if let Err(e) = Self::write_oom_files(exit_paths).await {
                                            error!("Writing OOM files failed: {:#}", e);
                                        }
                                        last_counter = 1;
                                        oom = true;
                                    }

                                    match tx.try_send(OOMEvent{ oom }) {
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
        watcher
            .unwatch(&memory_events_file_path)
            .context("unwatch memory events file")?;

        debug!("Done watching for ooms");

        Ok(())
    }

    /// Checks if a file does not exist on disk.
    async fn file_not_found(f: impl AsRef<Path>) -> bool {
        // TODO: use is_err_and if we can use Rust 1.70.0:
        // https://doc.rust-lang.org/std/result/enum.Result.html#method.is_err_and
        match tokio::fs::metadata(f).await {
            Err(e) => e.kind() == ErrorKind::NotFound,
            _ => false,
        }
    }

    async fn check_for_oom(
        memory_events_file_path: &Path,
        last_counter: u64,
    ) -> Result<(u64, bool)> {
        debug!(
            "Checking for possible OOM in {}",
            memory_events_file_path.display()
        );
        let mut new_counter: u64 = 0;
        let mut found_oom = false;
        let fp = File::open(memory_events_file_path).await.context(format!(
            "open memory events file: {}",
            memory_events_file_path.display()
        ))?;
        let reader = BufReader::new(fp);
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await.context("get next line")? {
            trace!(line);
            if let Some(counter) = line.strip_prefix("oom ").or(line.strip_prefix("oom_kill ")) {
                let counter = counter
                    .to_string()
                    .parse::<u64>()
                    .context("parse u64 counter")?;
                debug!("New oom counter: {counter}, last counter: {last_counter}",);
                if counter != last_counter {
                    debug!("Updating OOM counter to {counter}");
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
            task.await.context("wait for task to be finished")?;
        }
        Ok(())
    }

    async fn process_cgroup_subsystem_path_cgroup_v1(
        pid: u32,
        subsystem: &str,
    ) -> Result<Option<PathBuf>> {
        lazy_static! {
            static ref RE: Regex = Regex::new(".*:(.*):/(.*)").expect("could not compile regex");
        }

        if let Some(fp) = Self::try_open_cgroup_path(pid).await? {
            let reader = BufReader::new(fp);

            let mut lines = reader.lines();
            while let Some(line) = lines.next_line().await.context("read line from buffer")? {
                if let Some(caps) = RE.captures(&line) {
                    let system = caps
                        .get(1)
                        .context("no first capture group in regex match")?
                        .as_str();
                    let path = caps
                        .get(2)
                        .context("no second capture group in regex match")?
                        .as_str();
                    if system.contains(subsystem) || system.is_empty() {
                        return Ok(PathBuf::from(CGROUP_ROOT).join(subsystem).join(path).into());
                    }
                }
            }

            bail!("no path found")
        }

        Ok(None)
    }

    async fn process_cgroup_subsystem_path_cgroup_v2(pid: u32) -> Result<Option<PathBuf>> {
        lazy_static! {
            static ref RE: Regex = Regex::new(".*:.*:/(.*)").expect("could not compile regex");
        }

        if let Some(fp) = Self::try_open_cgroup_path(pid).await? {
            let mut buffer = String::new();
            let mut reader = BufReader::new(fp);

            reader
                .read_line(&mut buffer)
                .await
                .context("read line from buffer")?;

            if let Some(caps) = RE.captures(&buffer) {
                return Ok(Path::new(CGROUP_ROOT)
                    .join(
                        caps.get(1)
                            .context("no first capture group in regex match")?
                            .as_str(),
                    )
                    .into());
            }

            bail!("invalid cgroup")
        }

        Ok(None)
    }

    async fn try_open_cgroup_path(pid: u32) -> Result<Option<File>> {
        let cgroup_path = PathBuf::from("/proc").join(pid.to_string()).join("cgroup");
        debug!("Using cgroup path: {}", cgroup_path.display());

        match File::open(&cgroup_path).await {
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
