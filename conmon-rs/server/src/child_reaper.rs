//! Child process reaping and management.

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
        thread::spawn(move || {
            loop {
                let grandchildren = Arc::clone(&grandchildren);
                match waitpid(Pid::from_raw(-1), None) {
                    Ok(status) => {
                        if let WaitStatus::Exited(grandchild_pid, exit_status) = status {
                            // Immediately spawn a thread to reduce risk of dropping
                            // a SIGCHLD.
                            thread::spawn(move || {
                                if let Err(e) = Self::forget_grandchild(grandchildren, grandchild_pid, exit_status) {
                                    error!("Failed to reap grandchild for pid {}: {}", grandchild_pid, e);
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

    pub fn watch_grandchild(&self, id: String, pid: i32, exit_paths: Vec<PathBuf>) -> Result<()> {
        let locked_grandchildren = Arc::clone(&self.grandchildren);
        let mut map = locked_grandchildren.lock().map_err(|e| format_err!("lock grandchildren: {}", e))?;

        let reapable_grandchild = ReapableChild { id, exit_paths };
        if let Some(old) = map.insert(pid, reapable_grandchild) {
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

        let mut map = locked_map.lock().map_err(|e| format_err!("lock grandchildren: {}", e))?;
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
        for (pid, kc) in Arc::clone(&self.grandchildren).lock().map_err(|e| format_err!("lock grandchildren: {}", e))?.iter() {
            debug!("killing pid {} for container {}", pid, kc.id);
            kill(Pid::from_raw(*pid), s)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct ChildReaper {
    grandchildren: Arc<Mutex<HashMap<i32, ReapableChild>>>,
}

#[derive(Debug, Getters)]
pub struct ReapableChild {
    #[getset(get)]
    id: String,
    #[getset(get)]
    exit_paths: Vec<PathBuf>,
}

fn write_to_exit_paths(code: i32, paths: Vec<PathBuf>) -> Result<()> {
    let code_str = format!("{}", code);
    for path in paths {
        debug!("writing exit code {} to {}", code, path.display());
        File::create(path)?.write_all(code_str.as_bytes())?;
    }
    Ok(())
}
