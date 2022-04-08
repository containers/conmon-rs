//! Child process reaping and management.
use crate::{
    attach::Attach, child::Child, container_io::ContainerIO, container_log::SharedContainerLog,
};
use anyhow::{anyhow, format_err, Context, Result};
use getset::{CopyGetters, Getters, Setters};
use libc::pid_t;
use log::{debug, error};
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
    fs::File,
    io::Write,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::{
    fs,
    process::Command,
    sync::broadcast::{self, Receiver, Sender},
    task,
};

#[derive(Debug, Default)]
pub struct ChildReaper {
    grandchildren: Arc<Mutex<MultiMap<String, ReapableChild>>>,
}

macro_rules! lock {
    ($x:expr) => {
        $x.lock().map_err(|e| format_err!("{:#}", e))?
    };
}

impl ChildReaper {
    pub fn get(&self, id: &str) -> Result<ReapableChild> {
        let locked_grandchildren = Arc::clone(&self.grandchildren);
        let lock = lock!(locked_grandchildren);
        let r = lock.get(id).context("child not available")?.clone();
        drop(lock);
        Ok(r)
    }

    pub async fn create_child<P, I, S>(
        &self,
        cmd: P,
        args: I,
        container_io: &ContainerIO,
        pidfile: PathBuf,
    ) -> Result<u32>
    where
        P: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(cmd);
        cmd.args(args);
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn child process: {}")?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        match container_io {
            ContainerIO::Terminal(terminal) => {
                terminal
                    .wait_connected()
                    .context("wait for terminal socket connection")?;
            }
            ContainerIO::Streams(streams) => {
                streams.handle_stdio_receive(stdout, stderr);
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
            .context("grandchild pid read error")?
            .parse::<u32>()
            .context("grandchild pid parse error")?;

        Ok(grandchild_pid)
    }

    pub fn watch_grandchild(
        &self,
        child: Child,
        stop_tx: Option<Sender<()>>,
    ) -> Result<Receiver<i32>> {
        let locked_grandchildren = Arc::clone(&self.grandchildren);
        let mut map = lock!(locked_grandchildren);
        let reapable_grandchild = ReapableChild::from_child(&child);

        let (exit_tx, exit_rx) = reapable_grandchild.watch();

        map.insert(child.id().clone(), reapable_grandchild);
        let cleanup_grandchildren = locked_grandchildren.clone();
        let pid = child.pid();

        task::spawn(async move {
            exit_tx.subscribe().recv().await?;
            if let Some(stop_tx) = stop_tx {
                stop_tx.send(()).context("send message to stop channel")?;
            }
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
        let locked_grandchildren = Arc::clone(&self.grandchildren);
        for (_, grandchild) in lock!(locked_grandchildren).iter() {
            debug!("Killing pid {}", grandchild.pid);
            Self::kill_grandchild(grandchild.pid, s)?;
        }
        Ok(())
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
        kill(pid, s)?;
        Ok(())
    }
}

#[derive(Clone, CopyGetters, Debug, Getters, Setters)]
pub struct ReapableChild {
    #[getset(get)]
    exit_paths: Vec<PathBuf>,

    #[getset(get_copy)]
    pid: u32,

    #[getset(set = "pub")]
    attach: Option<Attach>,

    #[getset(get = "pub")]
    logger: SharedContainerLog,
}

impl ReapableChild {
    pub fn from_child(child: &Child) -> Self {
        Self {
            pid: child.pid(),
            exit_paths: child.exit_paths().clone(),
            attach: None,
            logger: child.logger().clone(),
        }
    }

    fn watch(&self) -> (Sender<i32>, Receiver<i32>) {
        let exit_paths = self.exit_paths().clone();
        let pid = self.pid();
        // Only one exit code will be written.
        let (exit_tx, exit_rx) = broadcast::channel(1);
        let exit_tx_clone = exit_tx.clone();

        task::spawn(async move {
            let exit_code = Self::wait_for_exit_code(pid);
            debug!("Sending exit code to channel: {}", exit_code);
            if exit_tx_clone.send(exit_code).is_err() {
                error!("Unable to send exit status");
            }
            Self::write_to_exit_paths(exit_code, &exit_paths).context("write exit paths")
        });
        (exit_tx, exit_rx)
    }

    fn wait_for_exit_code(pid: u32) -> i32 {
        const FAILED_EXIT_CODE: i32 = -3;
        for i in 1..10 {
            match waitpid(Pid::from_raw(pid as pid_t), None) {
                Ok(status) => {
                    if let WaitStatus::Exited(_, exit_code) = status {
                        return exit_code;
                    }
                    if let WaitStatus::Signaled(_, sig, _) = status {
                        return (sig as i32) + 128;
                    }
                    error!(
                        "Unable to get exit code because of unsupported wait status: {:?}",
                        status
                    );
                    return FAILED_EXIT_CODE;
                }
                Err(err) if err == Errno::EINTR => {
                    debug!("Failed to wait for pid on EINTR, retrying ({})", i);
                    continue;
                }
                Err(err) => {
                    error!("Unable to waitpid: {}", err);
                    return FAILED_EXIT_CODE;
                }
            };
        }
        error!("Timed out in waitpid");
        FAILED_EXIT_CODE
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
