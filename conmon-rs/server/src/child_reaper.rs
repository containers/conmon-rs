//! Child process reaping and management.
use crate::fd_mapping::{CommandFdExt, FdMapping};
use crate::{
    child::Child,
    container_io::{ContainerIO, ContainerIOType, SharedContainerIO},
    oom_watcher::OOMWatcher,
};
use anyhow::{Context, Result, bail};
use libc::pid_t;
use nix::{
    errno::Errno,
    sys::{
        signal::{Signal, kill},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::{Pid, getpgid},
};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fmt::{Debug, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    process::Stdio,
    str,
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
use tracing::{Instrument, debug, debug_span, error};

#[derive(Debug, Default)]
pub struct ChildReaper {
    grandchildren: Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>>,
}

impl ChildReaper {
    pub fn grandchildren(&self) -> &Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>> {
        &self.grandchildren
    }
}

/// first usable file descriptor after stdin, stdout and stderr
const FIRST_FD_AFTER_STDIO: RawFd = 3;

impl ChildReaper {
    pub fn get(&self, id: &str) -> Result<ReapableChild> {
        let lock = lock!(self.grandchildren());
        let r = lock
            .get(id)
            .and_then(|v| v.last())
            .context("child not available")?
            .clone();
        Ok(r)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_child<P, I, S>(
        &self,
        cmd: P,
        args: I,
        stdin: bool,
        container_io: &mut ContainerIO,
        pidfile: &Path,
        env_vars: Vec<(String, String)>,
        additional_fds: Vec<OwnedFd>,
    ) -> Result<(u32, CancellationToken)>
    where
        P: AsRef<OsStr> + Debug,
        I: IntoIterator<Item = S> + Debug,
        S: AsRef<OsStr>,
    {
        debug!("Running: {:?} {:?}", cmd, args);
        let mut cmd = Command::new(cmd);

        if stdin {
            cmd.stdin(Stdio::piped());
        }

        let mut child = cmd
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(env_vars)
            .fd_mappings(
                additional_fds
                    .iter()
                    .enumerate()
                    .map(|(i, fd)| FdMapping {
                        parent_fd: unsafe { OwnedFd::from_raw_fd(fd.as_raw_fd()) },
                        child_fd: i as RawFd + FIRST_FD_AFTER_STDIO,
                    })
                    .collect(),
            )?
            .spawn()
            .context("spawn child process: {}")?;

        debug!(
            "Running child on PID: {}",
            child.id().map_or("unknown".into(), |x| x.to_string())
        );

        // close file descriptors after spawn
        drop(additional_fds);

        let token = CancellationToken::new();

        match container_io.typ_mut() {
            ContainerIOType::Terminal(terminal) => {
                terminal
                    .wait_connected(stdin, token.clone())
                    .await
                    .context("wait for terminal socket connection")?;
            }
            ContainerIOType::Streams(streams) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let stdin = child.stdin.take();
                streams.handle_stdio_receive(stdin, stdout, stderr, token.clone());
            }
        };

        let output = child.wait_with_output().await?;

        if !output.status.success() {
            const BASE_ERR: &str = "child command exited with";

            let mut err_str = match output.status.code() {
                Some(code) => format!("{BASE_ERR}: {code}"),
                None => format!("{BASE_ERR} signal"),
            };

            if !output.stderr.is_empty() {
                write!(
                    err_str,
                    ": {}",
                    str::from_utf8(&output.stderr).context("convert stderr to utf8")?,
                )?;
            }
            // token must be cancelled here because the child hasn't been registered yet,
            // meaning there is no other entity that could cancel the read_loops.
            token.cancel();

            // Wait to ensure that all children do not become zombies.
            Self::check_child_processes();

            error!("Failed: {err_str}");
            bail!(err_str)
        }

        let grandchild_pid = fs::read_to_string(pidfile)
            .await
            .with_context(|| format!("grandchild pid read error {}", pidfile.display()))?
            .parse::<u32>()
            .with_context(|| format!("grandchild pid parse error {}", pidfile.display()))?;

        Ok((grandchild_pid, token))
    }

    fn check_child_processes() {
        debug!("Checking child processes");
        let pid = Pid::from_raw(-1);
        loop {
            match waitpid(pid, WaitPidFlag::WNOHANG.into()) {
                Ok(WaitStatus::Exited(p, code)) => {
                    debug!("PID {p} exited with status: {code}");
                    break;
                }
                Ok(WaitStatus::StillAlive) => {
                    debug!("PID {pid} is still in same state");
                    break;
                }
                Ok(_) => {
                    continue;
                }
                Err(Errno::EINTR) => {
                    debug!("Retrying on EINTR for PID {pid}");
                    continue;
                }
                Err(err) => {
                    error!("Unable to waitpid on {:#}", err);
                    break;
                }
            };
        }
    }

    pub fn watch_grandchild(
        &self,
        child: Child,
        leak_fds: Vec<OwnedFd>,
    ) -> Result<Receiver<ExitChannelData>> {
        let pid = child.pid;
        let (id, mut reapable_grandchild) = ReapableChild::from_child(child);

        // Create channels outside the lock to minimize contention
        let (exit_tx, exit_rx) = reapable_grandchild.watch()?;

        let locked_grandchildren = self.grandchildren();
        {
            let mut map = lock!(locked_grandchildren);
            map.entry(id).or_default().push(reapable_grandchild);
        }
        let cleanup_grandchildren = locked_grandchildren.clone();

        task::spawn(
            async move {
                exit_tx.subscribe().recv().await?;
                drop(leak_fds);
                Self::forget_grandchild(&cleanup_grandchildren, pid)
            }
            .instrument(debug_span!("watch_grandchild", pid)),
        );
        Ok(exit_rx)
    }

    fn forget_grandchild(
        locked_grandchildren: &Arc<Mutex<HashMap<Box<str>, Vec<ReapableChild>>>>,
        grandchild_pid: u32,
    ) -> Result<()> {
        let mut map = lock!(locked_grandchildren);
        for children in map.values_mut() {
            if let Some(pos) = children.iter().position(|v| v.pid == grandchild_pid) {
                children.swap_remove(pos);
                return Ok(());
            }
        }
        Ok(())
    }

    pub async fn kill_grandchildren(&self, s: Signal) -> Result<()> {
        debug!("Killing grandchildren");
        // Collect only PIDs and task handles to avoid cloning entire ReapableChild
        let children: Vec<_> = {
            let map = lock!(self.grandchildren);
            map.values()
                .flatten()
                .map(|gc| (gc.pid, gc.task.clone()))
                .collect()
        };
        for (pid, task) in children {
            let span = debug_span!("kill_grandchild", pid);
            let _enter = span.enter();
            debug!("Killing single grandchild");
            kill_grandchild(pid, s);
            if let Some(t) = task {
                let handle = lock!(t).take();
                if let Some(handle) = handle {
                    let close_span = debug_span!("close", signal = s.as_str());
                    async {
                        debug!("Waiting for task to close");
                        if let Err(e) = handle.await {
                            error!("Unable to close grandchild: {:#}", e)
                        }
                    }
                    .instrument(close_span)
                    .await;
                }
            }
            debug!("Done killing single grandchild");
        }
        debug!("Done killing all grandchildren");
        Ok(())
    }
}

pub fn kill_grandchild(raw_pid: u32, s: Signal) {
    let pid = Pid::from_raw(raw_pid as pid_t);
    if let Ok(pgid) = getpgid(Some(pid)) {
        // If process_group is 1, we will end up calling
        // kill(-1), which kills everything conmon is allowed to.
        let pgid = i32::from(pgid);

        #[allow(clippy::collapsible_if)]
        if pgid > 1 {
            if let Err(e) = kill(Pid::from_raw(-pgid), s) {
                error!(
                    raw_pid,
                    "Failed to get pgid, falling back to killing pid: {:#}", e
                );
            }
        }
    }
    if let Err(e) = kill(pid, s) {
        debug!("Failed killing pid: {:#}", e);
    }
}

type TaskHandle = Arc<Mutex<Option<JoinHandle<()>>>>;

#[derive(Clone, Debug)]
pub struct ReapableChild {
    exit_paths: Arc<[PathBuf]>,
    oom_exit_paths: Arc<[PathBuf]>,
    pid: u32,
    io: SharedContainerIO,
    timeout: Option<Instant>,
    token: CancellationToken,
    task: Option<TaskHandle>,
    cleanup_cmd: Arc<[String]>,
}

impl ReapableChild {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn io(&self) -> &SharedContainerIO {
        &self.io
    }

    pub fn timeout(&self) -> &Option<Instant> {
        &self.timeout
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }
}

#[derive(Clone, Debug)]
pub struct ExitChannelData {
    pub exit_code: i32,

    #[allow(dead_code)]
    pub oomed: bool,

    pub timed_out: bool,
}

impl ReapableChild {
    pub fn from_child(child: Child) -> (Box<str>, Self) {
        (
            child.id,
            Self {
                exit_paths: Arc::from(child.exit_paths),
                oom_exit_paths: Arc::from(child.oom_exit_paths),
                pid: child.pid,
                io: child.io,
                timeout: child.timeout,
                token: child.token,
                task: None,
                cleanup_cmd: Arc::from(child.cleanup_cmd),
            },
        )
    }

    fn watch(&mut self) -> Result<(Sender<ExitChannelData>, Receiver<ExitChannelData>)> {
        let exit_paths = self.exit_paths.clone();
        let oom_exit_paths = self.oom_exit_paths.clone();
        let pid = self.pid();
        // Only one exit code will be written.
        let (exit_tx, exit_rx) = broadcast::channel(1);
        let exit_tx_clone = exit_tx.clone();
        let timeout = *self.timeout();
        let stop_token = self.token().clone();
        let cleanup_cmd_raw = self.cleanup_cmd.clone();

        let task = task::spawn(
            async move {
                debug!("Running task");
                let mut exit_code: i32 = -1;
                let mut oomed = false;
                let mut timed_out = false;
                let (oom_tx, mut oom_rx) = tokio::sync::mpsc::channel(1);
                let oom_watcher = OOMWatcher::new(&stop_token, pid, &oom_exit_paths, oom_tx).await;

                let span = debug_span!("wait_for_exit_code");
                let wait_for_exit_code = task::spawn_blocking(move || {
                    let _enter = span.enter();
                    Self::wait_for_exit_code(&stop_token, pid)
                });

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
                        kill_grandchild(pid, Signal::SIGKILL);
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
                debug!(?exit_paths, "Write to exit paths");
                if let Err(e) = Self::write_to_exit_paths(exit_code, &exit_paths).await {
                    error!(pid, "Could not write exit paths: {:#}", e);
                }

                if !cleanup_cmd_raw.is_empty() {
                    Self::spawn_cleanup_process(&cleanup_cmd_raw).await;
                }

                debug!("Sending exit struct to channel: {:?}", exit_channel_data);
                if exit_tx_clone.send(exit_channel_data).is_err() {
                    debug!("Unable to send exit status");
                }
                debug!("Task done");
            }
            .instrument(debug_span!("watch", pid)),
        );

        self.task = Some(Arc::new(Mutex::new(Some(task))));

        Ok((exit_tx, exit_rx))
    }

    async fn spawn_cleanup_process(raw_cmd: &[String]) {
        let mut cleanup_cmd = Command::new(&raw_cmd[0]);

        cleanup_cmd.args(&raw_cmd[1..]);

        tokio::spawn(async move {
            match cleanup_cmd.status().await {
                Ok(status) => {
                    if !status.success() {
                        error!("Failed to execute cleanup command successfully: {}", status);
                    }
                }
                Err(e) => error!(
                    "Failed to spawn and execute cleanup command process successfully: {}",
                    e
                ),
            }
        });
    }

    fn wait_for_exit_code(token: &CancellationToken, pid: u32) -> i32 {
        debug!("Waiting for exit code");
        const FAILED_EXIT_CODE: i32 = -3;
        loop {
            match waitpid(Pid::from_raw(pid as pid_t), None) {
                Ok(WaitStatus::Exited(_, exit_code)) => {
                    debug!("Exited {}", exit_code);
                    token.cancel();
                    return exit_code;
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    debug!("Signaled: {sig}");
                    token.cancel();
                    return (sig as i32) + 128;
                }
                Ok(_) => {
                    continue;
                }
                Err(Errno::EINTR) => {
                    debug!("Failed to wait for pid on EINTR, retrying");
                    continue;
                }
                Err(err) => {
                    error!("Unable to waitpid on {:#}", err);
                    token.cancel();
                    return FAILED_EXIT_CODE;
                }
            };
        }
    }

    async fn write_to_exit_paths(code: i32, paths: &[PathBuf]) -> Result<()> {
        let code_str = format!("{code}");
        for path_buf in paths {
            let span = debug_span!("write_exit_path", path = %path_buf.display());
            async {
                debug!("Creating exit file");
                if let Ok(mut fp) = File::create(path_buf).await {
                    debug!(code, "Writing exit code to file");
                    if let Err(e) = fp.write_all(code_str.as_bytes()).await {
                        error!("Could not write exit file to path: {:#}", e);
                    }
                    debug!("Flushing file");
                    if let Err(e) = fp.flush().await {
                        error!("Unable to flush {}: {:#}", path_buf.display(), e);
                    }
                    debug!("Done writing exit file");
                }
            }
            .instrument(span)
            .await;
        }
        Ok(())
    }
}
