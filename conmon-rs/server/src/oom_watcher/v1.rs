//! The OOM notifications of a cgroup v1 memory controller.

use super::{OOMEvent, OOMWatcher};
use crate::async_fd::{self, Fd};
use anyhow::{Context, Result};
use nix::{
    sys::eventfd::{EfdFlags, EventFd},
    unistd::Pid,
};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::fd::{AsRawFd, RawFd},
    path::Path,
    pin::Pin,
    task::{Context as TaskContext, Poll, ready},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, ErrorKind, Interest, ReadBuf, unix::AsyncFd},
    time::{self, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// The state of a cgroup v1 `memory.oom_control` file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OomControl {
    /// Whether the OOM killer is disabled, tasks wait in the OOM then.
    kill_disable: bool,

    /// Whether the cgroup is in an OOM right now.
    under_oom: bool,

    /// Processes killed in this cgroup, kernels before 4.13 have no counter.
    oom_kill: Option<u64>,
}

impl OomControl {
    fn parse(content: &str) -> Self {
        let mut control = Self::default();
        for (key, value) in OOMWatcher::counters(content) {
            let value = value.ok();
            match key {
                "oom_kill_disable" => control.kill_disable = value.is_some_and(|v| v != 0),
                "under_oom" => control.under_oom = value.is_some_and(|v| v != 0),
                "oom_kill" => control.oom_kill = value,
                _ => {}
            }
        }
        control
    }
}

/// The outcome of confirming a cgroup v1 OOM event.
#[derive(Debug, PartialEq, Eq)]
enum Confirmation {
    Oom,
    /// The cgroup was already gone at the event, which removal signals.
    Removed,
    /// The cgroup went away while confirming.
    Gone,
    /// No kill showed up while the cgroup exists.
    Unconfirmed,
    /// The process exited while confirming.
    Exited,
}

/// The eventfd of a cgroup v1 OOM notification, read through the reactor of
/// tokio.
struct AsyncEventFd(AsyncFd<Fd<EventFd>>);

impl AsyncEventFd {
    fn new() -> Result<Self> {
        let fd = EventFd::from_flags(EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)
            .context("create eventfd")?;
        let fd = async_fd::register(fd, Interest::READABLE).context("register eventfd")?;
        Ok(Self(fd))
    }
}

impl AsRawFd for AsyncEventFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Reads the counter of the eventfd, which needs room for its 8 bytes.
impl AsyncRead for AsyncEventFd {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() < size_of::<u64>() {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::InvalidInput)));
        }
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;
            match guard.try_io(|fd| fd.get_ref().read().map_err(io::Error::from)) {
                Ok(result) => {
                    return Poll::Ready(result.map(|count| buf.put_slice(&count.to_ne_bytes())));
                }
                Err(_would_block) => {}
            }
        }
    }
}

impl OOMWatcher {
    pub(super) async fn oom_handling_cgroup_v1(
        token: &CancellationToken,
        pid: Pid,
    ) -> Result<Option<OOMEvent>> {
        // The setup reads procfs and cgroupfs inline, which does not block,
        // instead of adding a blocking task to every container and exec.
        let Some(memory_cgroup_path) = Self::process_cgroup_subsystem_path_cgroup_v1(pid, "memory")
            .context("process cgroup memory subsystem path")?
        else {
            debug!("Stopping OOM handler because no cgroup subsystem path exists");
            return Ok(None);
        };

        debug!(
            "Using memory cgroup v1 path: {}",
            memory_cgroup_path.display()
        );

        let memory_cgroup_file_oom_path = memory_cgroup_path.join("memory.oom_control");
        let event_control_path = memory_cgroup_path.join("cgroup.event_control");
        let path = memory_cgroup_file_oom_path.to_str();

        debug!(path, "Setup cgroup v1 oom handling");

        // Taken before registering, an OOM in between is found by the check on
        // exit. Unknown without the counter or if it cannot be read, then
        // every event counts.
        let baseline = match Self::read_oom_control(&memory_cgroup_file_oom_path) {
            Ok(control) => control.oom_kill,
            Err(e) => {
                debug!("Could not read initial OOM kills: {e:#}");
                None
            }
        };

        // Registering an event only needs the file open, like for conmon.
        // The kernel keeps what it needs of it once registered.
        let oom_cgroup_file =
            File::open(&memory_cgroup_file_oom_path).context("opening cgroup oom file")?;
        let oom_event_fd = AsyncEventFd::new().context("creating eventfd")?;
        OpenOptions::new()
            .write(true)
            .open(event_control_path)
            .context("opening cgroup event control file")?
            .write_all(
                format!(
                    "{} {}",
                    oom_event_fd.as_raw_fd(),
                    oom_cgroup_file.as_raw_fd()
                )
                .as_bytes(),
            )
            .context("writing control data")?;
        drop(oom_cgroup_file);

        debug!("Successfully setup cgroup v1 oom detection");

        let event =
            Self::watch_cgroup_v1(token, oom_event_fd, &memory_cgroup_file_oom_path, baseline)
                .await;
        Ok(Some(event))
    }

    /// Wait for an OOM of a cgroup v1 memory controller until `token` gets
    /// cancelled and return what was found.
    ///
    /// The eventfd also fires for an OOM of an ancestor cgroup, whose victim
    /// may be a sibling container, and once when the cgroup gets removed. So
    /// an event gets confirmed by the `oom_kill` counter of the cgroup. The
    /// kernel signals the eventfd before it kills, so the counter is read
    /// again a few times. It is not hierarchical either, a victim in a memcg
    /// nested inside the container is not counted, which `under_oom` covers.
    /// A missed OOM is worse than a rare false one, like conmon: an event
    /// counts without a known baseline or counter, and if the process exits
    /// while the event gets confirmed and the cgroup is still under OOM. An
    /// event which stayed unconfirmed, like an OOM of a sibling, does not
    /// count later, a normal stop would be taken for an OOM otherwise. If the
    /// cgroup goes away after an event, it may have been an OOM which killed
    /// the last process or the one of a sibling followed by a normal exit,
    /// see [`OOMEvent::possible_oom`]. The removal alone signals once.
    async fn watch_cgroup_v1(
        token: &CancellationToken,
        mut event_fd: impl AsyncRead + Unpin,
        oom_control: &Path,
        baseline: Option<u64>,
    ) -> OOMEvent {
        let mut buffer = [0u8; 8];
        loop {
            tokio::select! {
                // An OOM kill often ends the process, which cancels the token.
                // Handle the event first then.
                biased;
                res = event_fd.read(&mut buffer) => {
                    let count = match res {
                        Ok(n) if n == buffer.len() => u64::from_ne_bytes(buffer),
                        res => {
                            debug!("Unable to read OOM eventfd, checking on exit only: {res:?}");
                            break;
                        }
                    };
                    debug!("Got OOM event, count {count}");
                    match Self::confirm_oom_event(token, oom_control, baseline).await {
                        Confirmation::Oom => return OOMEvent::new(true),
                        Confirmation::Removed => {
                            // Removing the cgroup signals the eventfd once. A
                            // count above that means another event came
                            // before.
                            debug!("Memory cgroup removed");
                            return OOMEvent::possible(count > 1);
                        }
                        Confirmation::Gone => {
                            debug!("Memory cgroup removed while confirming the event");
                            return OOMEvent::possible(true);
                        }
                        Confirmation::Unconfirmed => {
                            debug!("OOM event without a kill in this cgroup");
                        }
                        Confirmation::Exited => {
                            return Self::check_oom_on_exit(oom_control, baseline, true);
                        }
                    }
                }
                () = token.cancelled() => {
                    debug!("Loop cancelled");
                    break;
                }
            }
        }
        token.cancelled().await;
        Self::check_oom_on_exit(oom_control, baseline, false)
    }

    /// The delays between the reads which confirm an OOM event.
    const CONFIRM_BACKOFF: [Duration; 4] = [
        Duration::from_millis(10),
        Duration::from_millis(50),
        Duration::from_millis(250),
        Duration::from_secs(1),
    ];

    /// Confirm an eventfd event of a cgroup v1 memory controller, see
    /// [`OOMWatcher::watch_cgroup_v1`].
    async fn confirm_oom_event(
        token: &CancellationToken,
        oom_control: &Path,
        baseline: Option<u64>,
    ) -> Confirmation {
        let Some(baseline) = baseline else {
            debug!("No OOM kill baseline, taking the event as OOM");
            return Confirmation::Oom;
        };
        let mut delays = Self::CONFIRM_BACKOFF.into_iter();
        let mut first = true;
        loop {
            match Self::read_oom_control(oom_control) {
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    return if first {
                        Confirmation::Removed
                    } else {
                        Confirmation::Gone
                    };
                }
                Err(e) => {
                    debug!("Unable to read OOM kills, taking the event as OOM: {e:#}");
                    return Confirmation::Oom;
                }
                Ok(control) => match control.oom_kill {
                    Some(kills) if kills > baseline => return Confirmation::Oom,
                    None => return Confirmation::Oom,
                    // Without the OOM killer the tasks wait in the OOM.
                    Some(_) if control.kill_disable && control.under_oom => {
                        return Confirmation::Oom;
                    }
                    Some(_) => match delays.next() {
                        Some(delay) => {
                            first = false;
                            tokio::select! {
                                () = time::sleep(delay) => {}
                                // The counter is final once the process exited.
                                () = token.cancelled() => return Confirmation::Exited,
                            }
                        }
                        // A victim in a nested memcg, which the counter of
                        // this cgroup does not count.
                        None if control.under_oom => return Confirmation::Oom,
                        None => return Confirmation::Unconfirmed,
                    },
                },
            }
        }
    }

    /// Check for an OOM kill a last time once the process exited. An event
    /// whose confirmation the exit interrupted counts if the cgroup is still
    /// under OOM, and may have been one if the cgroup is gone.
    fn check_oom_on_exit(oom_control: &Path, baseline: Option<u64>, confirming: bool) -> OOMEvent {
        match Self::read_oom_control(oom_control) {
            // Without a baseline the counter tells nothing, it may stem from
            // before the container. An event took that into account already.
            Ok(control) => OOMEvent::new(
                baseline
                    .zip(control.oom_kill)
                    .is_some_and(|(baseline, kills)| kills > baseline)
                    || (confirming && control.under_oom),
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => OOMEvent::possible(confirming),
            Err(e) => {
                debug!("Unable to read OOM kills on exit: {e:#}");
                OOMEvent::new(confirming)
            }
        }
    }

    /// Read a cgroup v1 `memory.oom_control` file. Inline like the other
    /// cgroupfs reads, they do not block.
    fn read_oom_control(path: &Path) -> io::Result<OomControl> {
        Ok(OomControl::parse(&std::fs::read_to_string(path)?))
    }
}

#[cfg(test)]
mod tests {
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
            task::spawn(async move {
                OOMWatcher::watch_cgroup_v1(&token, event_rx, &path, baseline).await
            })
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
}
