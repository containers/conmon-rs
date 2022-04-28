//! Child process reaping and management.
use crate::{
    child::Child,
    container_io::{ContainerIO, ContainerIOType, SharedContainerIO},
};
use anyhow::{anyhow, format_err, Context, Result};
use getset::{CopyGetters, Getters, Setters};
use libc::pid_t;
use log::{debug, error};
use multimap::MultiMap;
use nix::errno::Errno;
use nix::sys::wait::WaitPidFlag;
use nix::{
    sys::{
        signal::{kill, Signal},
        wait::{waitpid, WaitStatus},
    },
    unistd::{getpgid, Pid},
};
use std::{
    ffi::OsStr,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::{
    fs,
    process::Command,
    sync::broadcast::{self, Receiver, Sender},
    task,
    time::{self, Instant},
};

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
        let reapable_grandchild = ReapableChild::from_child(&child);

        let (exit_tx, exit_rx) = reapable_grandchild.watch()?;

        map.insert(child.id().clone(), reapable_grandchild);
        let cleanup_grandchildren = locked_grandchildren.clone();
        let pid = child.pid();

        task::spawn(async move {
            exit_tx.subscribe().recv().await?;
            Self::forget_grandchild(&cleanup_grandchildren, pid)
        });
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
        let locked_grandchildren = &self.grandchildren().clone();
        for (_, grandchild) in lock!(locked_grandchildren).iter() {
            debug!("Killing pid {}", grandchild.pid);
            kill_grandchild(grandchild.pid, s)?;
        }
        Ok(())
    }
}

pub fn kill_grandchild(pid: u32, s: Signal) -> Result<()> {
    let pid = Pid::from_raw(pid as pid_t);
    if let Ok(pgid) = getpgid(Some(pid)) {
        // If process_group is 1, we will end up calling
        // kill(-1), which kills everything conmon is allowed to.
        let pgid = i32::from(pgid);
        if pgid > 1 {
            match kill(Pid::from_raw(-pgid), s) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    error!("Failed to get pgid, falling back to killing pid {}", e);
                }
            }
        }
    }
    if let Err(e) = kill(pid, s) {
        debug!("Failed killing pid {} with error {}", pid, e);
    }
    Ok(())
}

#[derive(Clone, CopyGetters, Debug, Getters, Setters)]
pub struct ReapableChild {
    #[getset(get)]
    exit_paths: Vec<PathBuf>,

    #[getset(get_copy)]
    pid: u32,

    #[getset(get = "pub")]
    io: SharedContainerIO,

    #[getset(get = "pub")]
    timeout: Option<Instant>,
}

#[derive(Clone, CopyGetters, Debug, Getters, Setters)]
pub struct ExitChannelData {
    #[getset(get = "pub")]
    pub exit_code: i32,

    #[getset(get = "pub")]
    pub timed_out: bool,
}

impl ReapableChild {
    pub fn from_child(child: &Child) -> Self {
        Self {
            exit_paths: child.exit_paths().clone(),
            pid: child.pid(),
            io: child.io().clone(),
            timeout: *child.timeout(),
        }
    }

    fn watch(&self) -> Result<(Sender<ExitChannelData>, Receiver<ExitChannelData>)> {
        let exit_paths = self.exit_paths().clone();
        let pid = self.pid();
        // Only one exit code will be written.
        let (exit_tx, exit_rx) = broadcast::channel(1);
        let exit_tx_clone = exit_tx.clone();
        let timeout = *self.timeout();

        task::spawn(async move {
            let exit_code: i32;
            let mut timed_out = false;
            let wait_for_exit_code = task::spawn_blocking(move || Self::wait_for_exit_code(pid));
            if let Some(timeout) = timeout {
                match time::timeout_at(timeout, wait_for_exit_code).await {
                    Ok(status) => match status {
                        Ok(code) => exit_code = code,
                        Err(err) => {
                            return Err(err);
                        }
                    },
                    Err(_) => {
                        timed_out = true;
                        exit_code = -3;
                        let _ = kill_grandchild(pid, Signal::SIGKILL);
                    }
                }
            } else {
                match wait_for_exit_code.await {
                    Ok(code) => exit_code = code,
                    Err(_) => exit_code = -1,
                }
            }
            debug!(
                "Sending exit code to channel for pid {} : {}",
                pid, exit_code
            );
            let exit_channel_data = ExitChannelData {
                exit_code,
                timed_out,
            };
            if exit_tx_clone.send(exit_channel_data).is_err() {
                error!("Unable to send exit status");
            }
            let _ = Self::write_to_exit_paths(exit_code, &exit_paths).context("write exit paths");
            Ok(())
        });

        Ok((exit_tx, exit_rx))
    }

    fn wait_for_exit_code(pid: u32) -> i32 {
        const FAILED_EXIT_CODE: i32 = -3;
        loop {
            match waitpid(
                Pid::from_raw(pid as pid_t),
                Some(WaitPidFlag::WNOHANG | WaitPidFlag::__WALL),
            ) {
                Ok(WaitStatus::Exited(_, exit_code)) => {
                    return exit_code;
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
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
                    error!("Unable to waitpid on {}: {}", pid, err);
                    return FAILED_EXIT_CODE;
                }
            };
        }
    }

    fn write_to_exit_paths(code: i32, paths: &[PathBuf]) -> Result<()> {
        let code_str = format!("{}", code);
        for path in paths {
            debug!("Writing exit code {} to {}", code, path.display());
            File::create(path)?.write_all(code_str.as_bytes())?;
        }
        Ok(())
    }
}
