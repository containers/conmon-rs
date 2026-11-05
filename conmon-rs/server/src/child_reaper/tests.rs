use super::*;
use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, DRAIN_MAX_LOG_WAIT, DRAIN_MAX_WAIT, DRAIN_TIMEOUT, Message},
    container_log::ContainerLog,
    process::{
        Process, child_pid, children_by_scan, has_zombie, is_zombie, read_pid_file,
        wait_for_exit_code, zombie_children,
    },
    test_util::{Guarded, wait_for, wait_for_real},
};
use futures::FutureExt;
use nix::sys::{
    signal::kill,
    wait::{WaitStatus, waitpid},
};
use std::path::Path;
use std::process::Command;
use tokio::io::AsyncWriteExt;
use tokio::{
    select,
    task::{self, JoinHandle},
};

mod orphan_sweep;
mod output;
mod registration;
mod runtime_calls;
mod shutdown;
mod watcher;

/// A deadline of a runtime call which the tests do not reach.
fn distant_deadline() -> Instant {
    Instant::now() + Duration::from_secs(600)
}

fn is_gone(pid: Pid) -> bool {
    kill(pid, None).is_err()
}

/// A shell which nobody but the watcher waits for. It gets killed through
/// a pidfd if a test fails before it ended, which cannot hit a reused PID.
struct Spawned {
    pid: Pid,
    process: Process,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.process.signal(Signal::SIGKILL);
    }
}

fn spawn_sh(script: &str) -> Result<Spawned> {
    let pid = child_pid(Command::new("sh").args(["-c", script]).spawn()?.id())?;
    // Not reaped yet, so the PID is still its own.
    let process = Process::open(pid);
    Ok(Spawned { pid, process })
}

fn child(pid: Pid, timeout: Option<Instant>, exit_path: &Path, cleanup: &[&str]) -> Result<Child> {
    Ok(Child {
        id: "id".into(),
        kind: ChildKind::Container,
        pid,
        exit_paths: vec![exit_path.into()],
        oom_exit_paths: vec![],
        timeout,
        io: SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?),
        cleanup_cmd: cleanup.iter().map(ToString::to_string).collect(),
        token: CancellationToken::new(),
    })
}
