//! Child process reaping and management.

use crate::child::Child;
use anyhow::{bail, format_err, Result};
use getset::Getters;
use log::{debug, error};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::Pid;
use std::path::PathBuf;
use std::thread;
use std::{collections::HashMap, fs::File, io::Write, sync::Arc, sync::Mutex};

impl ChildReaper {
    pub fn init_reaper(&self) -> Result<()> {
        let grandchildren = Arc::clone(&self.grandchildren);
        let wait_lock = Arc::clone(&self.wait_lock);
        thread::spawn(move || {
            loop {
                // To prevent an error: "No child processes (os error 10)" when spawning new runtime processes,
                // we don't call waitpid until the runtime process has finished
                // Inspired by
                // https://github.com/kata-containers/kata-containers/blob/828a304883a4/src/agent/src/signal.rs#L27
                let wait_status = {
                    let _lock = wait_lock.lock();
                    waitpid(Pid::from_raw(-1), None)
                };

                match wait_status {
                    Ok(status) => {
                        if let WaitStatus::Exited(grandchild_pid, exit_status) = status {
                            let grandchildren = Arc::clone(&grandchildren);
                            thread::spawn(move || {
                                if let Err(e) = Self::forget_grandchild(
                                    grandchildren,
                                    grandchild_pid,
                                    exit_status,
                                ) {
                                    error!(
                                        "Failed to reap grandchild for pid {}: {}",
                                        grandchild_pid, e
                                    );
                                }
                            });
                        }
                    }

                    Err(err) => {
                        // TODO FIXME this busy loops right now while there are no grandchildren.
                        // There should be a broadcast mechanism so we only run this loop
                        // while there are grandchildren
                        if err != nix::errno::Errno::ECHILD {
                            error!("caught error in reading for sigchld {}", err);
                        }
                    }
                }
            }
        });
        Ok(())
    }

    pub fn create_child<P, I, S>(&self, cmd: P, args: I) -> Result<std::process::ExitStatus>
    where
        P: AsRef<std::ffi::OsStr>,
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut cmd = std::process::Command::new(cmd);
        cmd.args(args);

        // To prevent an error: "No child processes (os error 10)",
        // we prevent our reaper thread from calling waitpid until this child has completed.
        // Inspired by
        // https://github.com/kata-containers/kata-containers/blob/828a304883a4/src/agent/src/signal.rs#L27
        let wait_lock = Arc::clone(&self.wait_lock);
        let _lock = wait_lock
            .lock()
            .map_err(|e| format_err!("lock waitlock: {}", e))?;

        cmd.spawn()
            .map_err(|e| format_err!("spawn child process: {}", e))?
            .wait()
            .map_err(|e| format_err!("wait for child process: {}", e))
    }

    pub fn watch_grandchild(&self, child: &Child) -> Result<()> {
        let locked_grandchildren = Arc::clone(&self.grandchildren);
        let mut map = locked_grandchildren
            .lock()
            .map_err(|e| format_err!("lock grandchildren: {}", e))?;

        let reapable_grandchild = ReapableChild::from_child(child);
        if let Some(old) = map.insert(child.pid, reapable_grandchild) {
            bail!("Repeat PID for container {} found", old.id);
        }
        Ok(())
    }

    fn forget_grandchild(
        locked_map: Arc<Mutex<HashMap<i32, ReapableChild>>>,
        grandchild_pid: Pid,
        exit_status: i32,
    ) -> Result<()> {
        debug!("caught signal for pid {}", grandchild_pid);

        let mut map = locked_map
            .lock()
            .map_err(|e| format_err!("lock grandchildren: {}", e))?;
        let grandchild = match map.remove(&(i32::from(grandchild_pid))) {
            Some(c) => c,
            None => {
                // If we have an unregistered PID, then there's nothing to do.
                return Ok(());
            }
        };

        debug!(
            "PID {} associated with container {} exited with {}",
            grandchild_pid, grandchild.id, exit_status
        );
        if let Err(e) = write_to_exit_paths(exit_status, grandchild.exit_paths) {
            error!(
                "failed to write to exit paths process for id {} :{}",
                grandchild.id, e
            );
        }
        Ok(())
    }

    pub fn kill_grandchildren(&self, s: Signal) -> Result<()> {
        for (pid, kc) in Arc::clone(&self.grandchildren)
            .lock()
            .map_err(|e| format_err!("lock grandchildren: {}", e))?
            .iter()
        {
            debug!("killing pid {} for container {}", pid, kc.id);
            kill(Pid::from_raw(*pid), s)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct ChildReaper {
    grandchildren: Arc<Mutex<HashMap<i32, ReapableChild>>>,
    wait_lock: Arc<Mutex<bool>>,
}

#[derive(Debug, Getters)]
pub struct ReapableChild {
    #[getset(get)]
    id: String,
    #[getset(get)]
    exit_paths: Vec<PathBuf>,
}

impl ReapableChild {
    pub fn from_child(child: &Child) -> Self {
        Self {
            id: child.id.clone(),
            exit_paths: child.exit_paths.clone(),
        }
    }
}

fn write_to_exit_paths(code: i32, paths: Vec<PathBuf>) -> Result<()> {
    let code_str = format!("{}", code);
    for path in paths {
        debug!("writing exit code {} to {}", code, path.display());
        File::create(path)?.write_all(code_str.as_bytes())?;
    }
    Ok(())
}
