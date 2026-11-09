//! Starting an exec, of an exec_sync or a streaming one alike: the IO of the
//! process, the PID file of its runtime and the runtime call, up to the
//! registration of the process.

use super::{ChildReaper, RuntimeCall, StartedChild, WatchedExec};
use crate::{
    config::Config,
    conmon_capnp::conmon::CgroupManager,
    container_io::{ContainerIO, TempFile},
    container_log::ContainerLog,
    runtime_args::{ExecArgs, GenerateRuntimeArgs},
};
use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::time::Instant;

/// An exec while it gets started. An exec_sync and a streaming exec
/// set it up the same way, they differ in how they wait for the runtime and
/// take the output, see [`ExecStart::run`] and [`ExecStart::watch`].
pub(crate) struct ExecStart {
    /// The IO of the process, with the console socket of a terminal.
    pub(crate) container_io: ContainerIO,

    /// Where the runtime writes the PID of the process. Removed once the
    /// runtime wrote the PID, see [`ExecStart::watch`], or with a start which
    /// failed.
    pidfile: TempFile,
    args: ExecArgs,
    runtime: PathBuf,
}

impl ExecStart {
    /// Set up an exec of `command` in the container `id`, with a terminal if
    /// `tty`. The PID file in the runtime directory starts with
    /// `pidfile_prefix`, which tells the kind of exec apart.
    pub(crate) fn new(
        config: &Config,
        id: &str,
        tty: bool,
        command: &[String],
        cgroup_manager: CgroupManager,
        pidfile_prefix: &str,
    ) -> Result<Self> {
        let container_io =
            ContainerIO::new(tty, ContainerLog::new()).context("create container IO")?;
        let pidfile = ContainerIO::temp_file(Some(config.runtime_dir()), pidfile_prefix, ".pid");
        let args = GenerateRuntimeArgs {
            config,
            id,
            container_io: &container_io,
            pidfile: pidfile.path(),
            cgroup_manager,
        }
        .exec_args(command)
        .context("generate exec args")?;
        Ok(Self {
            container_io,
            pidfile,
            args,
            runtime: config.runtime().to_path_buf(),
        })
    }

    /// Run the runtime, with `stdin` for the process, and kill it at the
    /// `deadline` if it hangs, see [`ChildReaper::create_child`]. The caller
    /// reports a failure with [`ExecStart::runtime_error`].
    pub(crate) async fn run(
        &mut self,
        reaper: &ChildReaper,
        stdin: bool,
        deadline: Instant,
    ) -> Result<StartedChild> {
        let call = RuntimeCall {
            command_start: Some(self.args.command_start),
            stdin,
            ..RuntimeCall::new(
                &self.runtime,
                &self.args.args,
                self.pidfile.path(),
                deadline,
            )
        };
        reaper.create_child(call, &mut self.container_io).await
    }

    /// The error of a runtime which failed to start the process, with its
    /// stderr, see [`ContainerIO::runtime_error`].
    pub(crate) async fn runtime_error(&self, e: anyhow::Error) -> anyhow::Error {
        self.container_io.runtime_error(e).await
    }

    /// Register the process the runtime `started` under the `id` of its
    /// container, the process gets killed at `timeout`, see
    /// [`ChildReaper::watch_exec`].
    pub(crate) fn watch(
        self,
        reaper: &ChildReaper,
        id: Box<str>,
        started: StartedChild,
        timeout: Option<Instant>,
    ) -> Result<WatchedExec> {
        // The runtime wrote the PID, the file served its purpose.
        drop(self.pidfile);
        reaper.watch_exec(id, started, timeout, self.container_io)
    }
}
