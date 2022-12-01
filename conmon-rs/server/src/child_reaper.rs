//! Child process reaping and management.
use crate::{
    child::Child,
    container_io::{ContainerIO, ContainerIOType, SharedContainerIO},
    oom_watcher::OOMWatcher,
    pidwatch::{Event, PidWatch},
};
use anyhow::{bail, format_err, Context, Result};
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
    fmt::Write,
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
use tracing::{debug, debug_span, error, warn, Instrument};

#[derive(CopyGetters, Debug, Default, Getters, Setters)]
pub struct ChildReaper {
    #[getset(get)]
    grandchildren: Arc<Mutex<MultiMap<String, ReapableChild>>>,

    #[getset(get_copy, set = "pub")]
    use_ebpf: bool,
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
        stdin: bool,
        container_io: &mut ContainerIO,
        pidfile: &Path,
    ) -> Result<(u32, CancellationToken)>
    where
        P: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(cmd);

        if stdin {
            cmd.stdin(Stdio::piped());
        }

        let mut child = cmd
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn child process: {}")?;

        let token = CancellationToken::new();

        match container_io.typ_mut() {
            ContainerIOType::Terminal(ref mut terminal) => {
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
                Some(code) => format!("{}: {}", BASE_ERR, code),
                None => format!("{} signal", BASE_ERR),
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

            bail!(err_str)
        }

        let grandchild_pid = fs::read_to_string(pidfile)
            .await
            .context(format!("grandchild pid read error {}", pidfile.display()))?
            .parse::<u32>()
            .context(format!("grandchild pid parse error {}", pidfile.display()))?;

        Ok((grandchild_pid, token))
    }

    pub fn watch_grandchild(&self, child: Child) -> Result<Receiver<ExitChannelData>> {
        let locked_grandchildren = &self.grandchildren().clone();
        let mut map = lock!(locked_grandchildren);
        let mut reapable_grandchild = ReapableChild::from_child(&child, self.use_ebpf());

        let (exit_tx, exit_rx) = reapable_grandchild.watch()?;

        map.insert(child.id().clone(), reapable_grandchild);
        let cleanup_grandchildren = locked_grandchildren.clone();
        let pid = child.pid();

        task::spawn(
            async move {
                exit_tx.subscribe().recv().await?;
                Self::forget_grandchild(&cleanup_grandchildren, pid)
            }
            .instrument(debug_span!("watch_grandchild", pid)),
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
        debug!("Killing grandchildren");
        let grandchildren = lock!(self.grandchildren);
        let grandchildren_iter = grandchildren.iter();
        for (_, grandchild) in grandchildren_iter {
            let span = debug_span!("kill_grandchild", pid = grandchild.pid);
            let _enter = span.enter();
            debug!("Killing single grandchild");
            kill_grandchild(grandchild.pid, s);
            futures::executor::block_on(
                async {
                    if let Err(e) = grandchild.close().await {
                        error!("Unable to close grandchild: {:#}", e)
                    }
                }
                .instrument(debug_span!("close", signal = s.as_str())),
            );
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

type TaskHandle = Arc<Mutex<Option<Vec<JoinHandle<Result<()>>>>>>;

#[derive(Clone, CopyGetters, Debug, Getters, Setters)]
pub struct ReapableChild {
    #[getset(get_copy)]
    use_ebpf: bool,

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

    #[getset(get = "pub")]
    cleanup_cmd: Vec<String>,
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
    pub fn from_child(child: &Child, use_ebpf: bool) -> Self {
        Self {
            use_ebpf,
            exit_paths: child.exit_paths().clone(),
            oom_exit_paths: child.oom_exit_paths().clone(),
            pid: child.pid(),
            io: child.io().clone(),
            timeout: *child.timeout(),
            token: child.token().clone(),
            task: None,
            cleanup_cmd: child.cleanup_cmd().to_vec(),
        }
    }

    pub async fn close(&self) -> Result<()> {
        debug!("Waiting for tasks to close");
        if let Some(t) = self.task.clone() {
            let tasks = lock!(t).take().context("no tasks available")?;
            for t in tasks.into_iter() {
                debug!("Task await");
                if let Err(e) = t.await {
                    warn!("Unable to wait for task: {:#}", e)
                }
                debug!("Task finished");
            }
        }
        debug!("All tasks done");
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
        let mut cleanup_cmd_raw = self.cleanup_cmd().clone();
        let use_ebpf = self.use_ebpf();

        let task = task::spawn(
            async move {
                debug!("Running task");
                let mut exit_code: i32 = -1;
                let mut oomed = false;
                let mut timed_out = false;
                let (oom_tx, mut oom_rx) = tokio::sync::mpsc::channel(1);

                let (mut oom_watcher, wait_for_exit_code, mut pidwatch_rx) = if use_ebpf {
                    (
                        None,
                        None,
                        PidWatch::new(pid)
                            .run()
                            .await
                            .context("create PID watcher")?
                            .into(),
                    )
                } else {
                    let oom_watcher =
                        OOMWatcher::new(&stop_token, pid, &oom_exit_paths, oom_tx).await;

                    let span = debug_span!("wait_for_exit_code");
                    let wait_for_exit_code = task::spawn_blocking(move || {
                        let _enter = span.enter();
                        Self::wait_for_exit_code(&stop_token, pid)
                    });

                    (oom_watcher.into(), wait_for_exit_code.into(), None)
                };

                let closure = async {
                    if let Some(pidwatch_rx) = pidwatch_rx.as_mut() {
                        match pidwatch_rx.recv().await {
                            Some(Event::Exited(c)) => exit_code = c,
                            Some(Event::Signaled(c)) => exit_code = c,
                            Some(Event::OOMKilled) => {
                                oomed = true;
                                Self::write_oom_files(oom_exit_paths)
                                    .await
                                    .context("write OOM files")?;
                            }
                            Some(Event::Err(e)) => bail!("Unable to watch PID: {:#}", e),
                            None => (),
                        }
                    } else if let Some(wait_for_exit_code) = wait_for_exit_code {
                        let (code, oom) = tokio::join!(wait_for_exit_code, oom_rx.recv());
                        if let Ok(code) = code {
                            exit_code = code;
                        }
                        if let Some(event) = oom {
                            oomed = event.oom;
                        }
                    }

                    Ok::<_, anyhow::Error>(())
                };

                if let Some(timeout) = timeout {
                    if let Err(e) = time::timeout_at(timeout, closure).await {
                        error!("Unable to wait for timeout: {:#}", e);
                        timed_out = true;
                        exit_code = -3;
                        kill_grandchild(pid, Signal::SIGKILL);
                    }
                } else {
                    closure.await.context("wait for closure")?;
                }

                if let Some(oom_watcher) = oom_watcher.take() {
                    oom_watcher.stop().await;
                }

                let exit_channel_data = ExitChannelData {
                    exit_code,
                    oomed,
                    timed_out,
                };
                debug!(
                    "Write to exit paths: {}",
                    exit_paths
                        .iter()
                        .map(|x| x.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if let Err(e) = Self::write_to_exit_paths(exit_code, &exit_paths).await {
                    error!(pid, "Could not write exit paths: {:#}", e);
                }

                if !cleanup_cmd_raw.is_empty() {
                    Self::spawn_cleanup_process(&mut cleanup_cmd_raw).await;
                }

                debug!("Sending exit struct to channel: {:?}", exit_channel_data);
                exit_tx_clone
                    .send(exit_channel_data)
                    .context("send exit channel data")?;

                debug!("Task done");
                Ok(())
            }
            .instrument(debug_span!("watch", pid)),
        );

        let tasks = Arc::new(Mutex::new(Some(Vec::new())));
        lock!(tasks)
            .as_mut()
            .context("no tasks available")?
            .push(task);
        self.task = Some(tasks);

        Ok((exit_tx, exit_rx))
    }

    async fn spawn_cleanup_process(raw_cmd: &mut Vec<String>) {
        let mut cleanup_cmd = Command::new(raw_cmd.remove(0));

        raw_cmd.iter().for_each(|arg| {
            cleanup_cmd.arg(arg);
        });

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
                    debug!("Signaled");
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
        let paths = paths.to_owned();
        let tasks: Vec<_> = paths
            .into_iter()
            .map(|path_buf| {
                let path = path_buf.display().to_string();
                tokio::spawn(
                    async move {
                        let code_str = format!("{}", code);
                        debug!("Creating exit file");
                        if let Ok(mut fp) = File::create(&path_buf).await {
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
                    .instrument(debug_span!("write_exit_path", path)),
                )
            })
            .collect();

        for task in tasks {
            task.await?;
        }

        Ok(())
    }

    pub async fn write_oom_files(exit_paths: Vec<PathBuf>) -> Result<()> {
        for task in exit_paths
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
            .collect::<Vec<_>>()
        {
            task.await.context("wait for task to be finished")?;
        }
        Ok(())
    }
}
