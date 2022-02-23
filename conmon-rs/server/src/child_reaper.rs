//! Child process reaping and management.
use crate::{child::Child, console::Console};
use anyhow::{format_err, Context, Result};
use log::{debug, error};
use multimap::MultiMap;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::Pid;
use std::path::PathBuf;
use std::sync::Mutex;
use std::{fs::File, io::Write, sync::Arc};

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
        console: Option<Console>,
        pidfile: PathBuf,
    ) -> Result<i32>
    where
        P: AsRef<std::ffi::OsStr>,
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut cmd = tokio::process::Command::new(cmd);
        cmd.args(args);
        cmd.spawn()
            .context("spawn child process: {}")?
            .wait()
            .await?;

        if let Some(console) = console {
            let _ = console
                .wait_connected()
                .context("wait for console socket connection");
        }

        let grandchild_pid = tokio::fs::read_to_string(pidfile)
            .await?
            .parse::<i32>()
            .context("grandchild pid parse error")?;

        Ok(grandchild_pid)
    }

    pub fn watch_grandchild(&self, child: Child) -> Result<()> {
        let locked_grandchildren = Arc::clone(&self.grandchildren);
        let mut map = lock!(locked_grandchildren);
        let reapable_grandchild = ReapableChild::from_child(&child);
        let killed_channel = reapable_grandchild.watch();
        map.insert(child.id, reapable_grandchild);
        let cleanup_grandchildren = locked_grandchildren.clone();
        let pid = child.pid;
        tokio::task::spawn(async move {
            killed_channel.await.expect("error on channel");
            if let Err(e) = Self::forget_grandchild(&cleanup_grandchildren, pid) {
                error!("error forgetting grandchild {}", e);
            }
        });
        Ok(())
    }

    fn forget_grandchild(
        locked_grandchildren: &Arc<Mutex<MultiMap<String, ReapableChild>>>,
        grandchild_pid: i32,
    ) -> Result<()> {
        let mut map = lock!(locked_grandchildren);
        map.retain(|_, v| v.pid == grandchild_pid);
        Ok(())
    }

    pub fn kill_grandchildren(&self, s: Signal) -> Result<()> {
        let locked_grandchildren = Arc::clone(&self.grandchildren);
        for (_, grandchild) in lock!(locked_grandchildren).iter() {
            debug!("killing pid {}", grandchild.pid);
            kill(Pid::from_raw(grandchild.pid), s)?;
        }
        Ok(())
    }
}

#[derive(Default, Debug, Clone)]
pub struct ReapableChild {
    pub exit_paths: Vec<PathBuf>,
    pub pid: i32,
}

impl ReapableChild {
    pub fn from_child(child: &Child) -> Self {
        Self {
            pid: child.pid,
            exit_paths: child.exit_paths.clone(),
        }
    }

    fn watch(&self) -> tokio::sync::oneshot::Receiver<()> {
        let exit_paths = self.exit_paths.clone();
        let pid = self.pid;
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::task::spawn_blocking(move || {
            let wait_status = waitpid(Pid::from_raw(pid), None);
            match wait_status {
                Ok(status) => {
                    if let WaitStatus::Exited(_, exit_status) = status {
                        let _ = write_to_exit_paths(exit_status, &exit_paths);
                    }
                }
                Err(err) => {
                    // TODO perhaps writing to the exit file anyway?
                    // TODO maybe retry the waitpid?
                    if err != nix::errno::Errno::ECHILD {
                        error!("caught error in reading for sigchld {}", err);
                    }
                }
            };
            if tx.send(()).is_err() {
                error!("the receiver dropped the watch")
            }
        });
        rx
    }
}

fn write_to_exit_paths(code: i32, paths: &[PathBuf]) -> Result<()> {
    let code_str = format!("{}", code);
    for path in paths {
        debug!("writing exit code {} to {}", code, path.display());
        File::create(path)?.write_all(code_str.as_bytes())?;
    }
    Ok(())
}
