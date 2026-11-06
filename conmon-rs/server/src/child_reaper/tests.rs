use super::*;
use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, DRAIN_MAX_LOG_WAIT, DRAIN_MAX_WAIT, DRAIN_TIMEOUT, Message},
    container_log::ContainerLog,
    process::{
        Process, child_pid, children_by_scan, has_zombie, is_zombie, read_pid_file,
        wait_for_exit_code, zombie_children,
    },
    test_util::{Guarded, Spawned, container_child, spawn_sh, wait_for, wait_for_real},
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

fn child(pid: Pid, timeout: Option<Instant>, exit_path: &Path) -> Result<Child> {
    Ok(Child {
        pid,
        exit_paths: vec![exit_path.into()],
        timeout,
        ..container_child(false)?
    })
}
