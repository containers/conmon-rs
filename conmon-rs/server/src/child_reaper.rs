//! Child process reaping and management.
use crate::{child::Child, console::Console};
use anyhow::{bail, format_err, Context, Result};
use libc::pid_t;
use log::debug;
use multimap::MultiMap;
use nix::errno::Errno;
use nix::{
    sys::{
        signal::{kill, Signal},
        wait::{waitpid, WaitStatus},
    },
    unistd::Pid,
};
use std::{fs::File, io::Write, path::PathBuf, sync::Arc, sync::Mutex};
use tokio::{
    fs,
    process::Command,
    sync::oneshot::{self, Receiver, Sender},
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
    pub fn get(&self, id: String) -> Result<ReapableChild> {
        let locked_grandchildren = Arc::clone(&self.grandchildren);
        let lock = lock!(locked_grandchildren);
        let r = lock.get(&id).context("")?.clone();
        drop(lock);
        Ok(r)
    }

    pub async fn create_child<P, I, S>(
        &self,
        cmd: P,
        args: I,
        console: Option<&Console>,
        pidfile: PathBuf,
    ) -> Result<u32>
    where
        P: AsRef<std::ffi::OsStr>,
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut cmd = Command::new(cmd);
        cmd.args(args);
        cmd.spawn()
            .context("spawn child process: {}")?
            .wait()
            .await?;

        if let Some(console) = console {
            console
                .wait_connected()
                .context("wait for console socket connection")?;
        }

        let grandchild_pid = fs::read_to_string(pidfile)
            .await?
            .parse::<u32>()
            .context("grandchild pid parse error")?;

        Ok(grandchild_pid)
    }

    pub fn watch_grandchild(&self, child: Child) -> Result<Receiver<i32>> {
        let locked_grandchildren = Arc::clone(&self.grandchildren);
        let mut map = lock!(locked_grandchildren);
        let reapable_grandchild = ReapableChild::from_child(&child);

        let (exit_tx, exit_rx) = oneshot::channel();
        let killed_channel = reapable_grandchild.watch(exit_tx);

        map.insert(child.id, reapable_grandchild);
        let cleanup_grandchildren = locked_grandchildren.clone();
        let pid = child.pid;

        task::spawn(async move {
            killed_channel.await?;
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
            debug!("killing pid {}", grandchild.pid);
            kill(Pid::from_raw(grandchild.pid as pid_t), s)?;
        }
        Ok(())
    }
}

#[derive(Default, Debug, Clone)]
pub struct ReapableChild {
    pub exit_paths: Vec<PathBuf>,
    pub pid: u32,
}

impl ReapableChild {
    pub fn from_child(child: &Child) -> Self {
        Self {
            pid: child.pid,
            exit_paths: child.exit_paths.clone(),
        }
    }

    fn watch(&self, exit_tx: Sender<i32>) -> Receiver<()> {
        let exit_paths = self.exit_paths.clone();
        let pid = self.pid;
        let (tx, rx) = oneshot::channel();

        task::spawn_blocking(move || {
            let wait_status = waitpid(Pid::from_raw(pid as pid_t), None);
            match wait_status {
                Ok(status) => {
                    if let WaitStatus::Exited(_, exit_code) = status {
                        Self::write_to_exit_paths(exit_code, &exit_paths)?;

                        debug!("Sending exit code to channel: {}", exit_code);
                        if exit_tx.send(exit_code).is_err() {
                            bail!("unable to send exit status")
                        }
                    }
                }
                Err(err) => {
                    // TODO perhaps writing to the exit file anyway?
                    // TODO maybe retry the waitpid?
                    if err != Errno::ECHILD {
                        bail!("caught error in reading for sigchld {}", err);
                    }
                }
            };
            if tx.send(()).is_err() {
                bail!("the receiver dropped the watch")
            }
            Ok(())
        });
        rx
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
