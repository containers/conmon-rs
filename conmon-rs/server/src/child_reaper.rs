//! Child process reaping and management.
use crate::{
    child::Child,
    container_io::{ContainerIO, ContainerIOType, SharedContainerIO},
    oom_watcher::OOMWatcher,
};
use anyhow::{anyhow, format_err, Context, Result};
use getset::{CopyGetters, Getters, Setters};
use libc::pid_t;
use multimap::MultiMap;
use nix::errno::Errno;
use nix::{
    sys::{
        signal::{kill, Signal},
        wait::{waitpid, WaitStatus},
    },
    unistd::{getpgid, Pid},
};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    process::Command,
    sync::broadcast::{self, Receiver, Sender},
    task::{self, JoinHandle},
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, error, Instrument};

#[derive(Debug, Default, Getters)]
pub struct ChildReaper {
    #[getset(get)]
    grandchildren: Arc<Mutex<MultiMap<String, ReapableChild>>>,
}

macro_rules! lock {
    ($x:expr) => {
        $x.lock().map_err(|e| format_err!("{:#}", e))?
    };
}

impl ChildReaper {
    pub fn get(&self, id: &str) -> Result<ReapableChild> {
        let locked_grandchildren = &self.grandchildren().clone();
        let lock = lock!(locked_grandchildren);
        let r = lock.get(id).context("child not available")?.clone();
        drop(lock);
        Ok(r)
    }

    pub async fn create_child<P, I, S>(
        &self,
        cmd: P,
        args: I,
        container_io: &mut ContainerIO,
        pidfile: &Path,
    ) -> Result<u32>
    where
        P: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(cmd);
        cmd.args(args);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn child process: {}")?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();

        match container_io.typ_mut() {
            ContainerIOType::Terminal(ref mut terminal) => {
                terminal
                    .wait_connected()
                    .await
                    .context("wait for terminal socket connection")?;
            }
            ContainerIOType::Streams(streams) => {
                streams.handle_stdio_receive(stdin, stdout, stderr);
            }
        }
        let status = child.wait().await?;

        if !status.success() {
            let code_str = match status.code() {
                Some(code) => format!("Child command exited with status: {}", code),
                None => "Child command exited with signal".to_string(),
            };
            // TODO Eventually, we'll need to read the stderr from the command
            // to get the actual message runc returned.
            return Err(anyhow!(code_str));
        }

        let grandchild_pid = fs::read_to_string(pidfile)
            .await
            .context(format!("grandchild pid read error {}", pidfile.display()))?
            .parse::<u32>()
            .context(format!("grandchild pid parse error {}", pidfile.display()))?;

        Ok(grandchild_pid)
    }

    pub fn watch_grandchild(&self, child: Child) -> Result<Receiver<ExitChannelData>> {
        let locked_grandchildren = &self.grandchildren().clone();
        let mut map = lock!(locked_grandchildren);
        let mut reapable_grandchild = ReapableChild::from_child(&child);

        let (exit_tx, exit_rx) = reapable_grandchild.watch()?;

        map.insert(child.id().clone(), reapable_grandchild);
        let cleanup_grandchildren = locked_grandchildren.clone();
        let pid = child.pid();

        task::spawn(
            async move {
                exit_tx.subscribe().recv().await?;
                Self::forget_grandchild(&cleanup_grandchildren, pid)
            }
            .instrument(debug_span!("watch_grandchild")),
        );
        Ok(exit_rx)
    }

    fn forget_grandchild(
        locked_grandchildren: &Arc<Mutex<MultiMap<String, ReapableChild>>>,
        grandchild_pid: u32,
    ) -> Result<()> {
        let mut map = lock!(locked_grandchildren);
        map.retain(|_, v| v.pid == grandchild_pid);
        Ok(())
    }

    pub fn kill_grandchildren(&self, s: Signal) -> Result<()> {
        for (_, grandchild) in lock!(self.grandchildren).iter() {
            debug!(grandchild.pid, "killing grandchild");
            let _ = kill_grandchild(grandchild.pid, s);
            futures::executor::block_on(async {
                if let Err(e) = grandchild.close().await {
                    error!("Unable to close grandchild: {}", e)
                }
            });
        }
        Ok(())
    }
}

pub fn kill_grandchild(raw_pid: u32, s: Signal) -> Result<()> {
    let pid = Pid::from_raw(raw_pid as pid_t);
    if let Ok(pgid) = getpgid(Some(pid)) {
        // If process_group is 1, we will end up calling
        // kill(-1), which kills everything conmon is allowed to.
        let pgid = i32::from(pgid);
        if pgid > 1 {
            match kill(Pid::from_raw(-pgid), s) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    error!(
                        raw_pid,
                        "Failed to get pgid, falling back to killing pid {}", e
                    );
                }
            }
        }
    }
    if let Err(e) = kill(pid, s) {
        debug!(raw_pid, "Failed killing pid with error {}", e);
    }
    Ok(())
}

type TaskHandle = Arc<Mutex<Option<Vec<JoinHandle<()>>>>>;

#[derive(Clone, CopyGetters, Debug, Getters, Setters)]
pub struct ReapableChild {
    #[getset(get)]
    exit_paths: Vec<PathBuf>,

    #[getset(get)]
    oom_exit_paths: Vec<PathBuf>,

    #[getset(get_copy)]
    pid: u32,

    #[getset(get = "pub")]
    io: SharedContainerIO,

    #[getset(get = "pub")]
    timeout: Option<Instant>,

    #[getset(get = "pub")]
    token: CancellationToken,

    task: Option<TaskHandle>,
}

#[derive(Clone, CopyGetters, Debug, Getters, Setters)]
pub struct ExitChannelData {
    #[getset(get = "pub")]
    pub exit_code: i32,

    #[getset(get = "pub")]
    pub oomed: bool,

    #[getset(get = "pub")]
    pub timed_out: bool,
}

impl ReapableChild {
    pub fn from_child(child: &Child) -> Self {
        Self {
            exit_paths: child.exit_paths().clone(),
            oom_exit_paths: child.oom_exit_paths().clone(),
            pid: child.pid(),
            io: child.io().clone(),
            timeout: *child.timeout(),
            token: CancellationToken::new(),
            task: None,
        }
    }

    pub async fn close(&self) -> Result<()> {
        debug!("{}: grandchild close", self.pid);
        self.token.cancel();
        if let Some(t) = self.task.clone() {
            for t in lock!(t).take().context("no tasks available")?.into_iter() {
                debug!("{}: grandchild await", self.pid);
                let _ = t.await;
            }
        }
        Ok(())
    }

    fn watch(&mut self) -> Result<(Sender<ExitChannelData>, Receiver<ExitChannelData>)> {
        let exit_paths = self.exit_paths().clone();
        let oom_exit_paths = self.oom_exit_paths().clone();
        let pid = self.pid();
        // Only one exit code will be written.
        let (exit_tx, exit_rx) = broadcast::channel(1);
        let exit_tx_clone = exit_tx.clone();
        let timeout = *self.timeout();
        let stop_token = self.token().clone();

        let task = task::spawn(
            async move {
                let mut exit_code: i32 = -1;
                let mut oomed = false;
                let mut timed_out = false;
                let (oom_tx, mut oom_rx) = tokio::sync::mpsc::channel(1);
                let oom_watcher = OOMWatcher::new(&stop_token, pid, &oom_exit_paths, oom_tx).await;
                let wait_for_exit_code =
                    task::spawn_blocking(move || Self::wait_for_exit_code(&stop_token, pid));
                let closure = async {
                    let (code, oom) = tokio::join!(wait_for_exit_code, oom_rx.recv());
                    if let Ok(code) = code {
                        exit_code = code;
                    }
                    if let Some(event) = oom {
                        oomed = event.oom;
                    }
                };
                if let Some(timeout) = timeout {
                    if time::timeout_at(timeout, closure).await.is_err() {
                        timed_out = true;
                        exit_code = -3;
                        let _ = kill_grandchild(pid, Signal::SIGKILL);
                    }
                } else {
                    closure.await;
                }
                oom_watcher.stop().await;
                let exit_channel_data = ExitChannelData {
                    exit_code,
                    oomed,
                    timed_out,
                };
                debug!(
                    pid,
                    "sending exit struct to channel : {:?}", exit_channel_data
                );
                if exit_tx_clone.send(exit_channel_data).is_err() {
                    debug!(pid, "Unable to send exit status");
                }
                debug!(pid, "write to exit paths");
                if let Err(e) = Self::write_to_exit_paths(exit_code, &exit_paths).await {
                    error!(pid, "could not write exit paths: {}", e);
                }
            }
            .instrument(debug_span!("watch")),
        );

        let tasks = Arc::new(Mutex::new(Some(Vec::new())));
        lock!(tasks)
            .as_mut()
            .context("no tasks available")?
            .push(task);
        self.task = Some(tasks);

        Ok((exit_tx, exit_rx))
    }

    fn wait_for_exit_code(token: &CancellationToken, pid: u32) -> i32 {
        const FAILED_EXIT_CODE: i32 = -3;
        loop {
            match waitpid(Pid::from_raw(pid as pid_t), None) {
                Ok(WaitStatus::Exited(_, exit_code)) => {
                    debug!(pid, "Exited {}", exit_code);
                    token.cancel();
                    return exit_code;
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    debug!(pid, "Signaled");
                    token.cancel();
                    return (sig as i32) + 128;
                }
                Ok(_) => {
                    continue;
                }
                Err(Errno::EINTR) => {
                    debug!(pid, "Failed to wait for pid on EINTR, retrying");
                    continue;
                }
                Err(err) => {
                    error!(pid, "Unable to waitpid on {}", err);
                    token.cancel();
                    return FAILED_EXIT_CODE;
                }
            };
        }
    }

    async fn write_to_exit_paths(code: i32, paths: &[PathBuf]) -> Result<()> {
        let paths = paths.to_owned();
        let tasks: Vec<_> = paths
            .into_iter()
            .map(|path| {
                tokio::spawn(async move {
                    let code_str = format!("{}", code);
                    if let Ok(mut fp) = File::create(&path).await {
                        if let Err(e) = fp.write_all(code_str.as_bytes()).await {
                            error!("could not write exit file to path {} {}", path.display(), e);
                        }
                    }
                })
            })
            .collect();

        for task in tasks {
            task.await?;
        }

        Ok(())
    }
}
