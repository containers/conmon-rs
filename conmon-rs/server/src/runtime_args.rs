//! The command line of the OCI runtime for a create, a restore or an exec.

use crate::{
    config::{Config, LogLevel},
    conmon_capnp::conmon::CgroupManager,
    container_io::ContainerIO,
};
use anyhow::{Result, bail};
use std::{fmt, path::Path};
use tracing::debug;

/// What the command line of the OCI runtime gets generated from.
#[derive(Clone, Copy)]
pub(crate) struct GenerateRuntimeArgs<'a> {
    pub(crate) config: &'a Config,
    pub(crate) id: &'a str,
    pub(crate) container_io: &'a ContainerIO,
    pub(crate) pidfile: &'a Path,
    pub(crate) cgroup_manager: CgroupManager,
}

impl GenerateRuntimeArgs<'_> {
    const SYSTEMD_CGROUP_ARG: &'static str = "--systemd-cgroup";
    const LOG_LEVEL_FLAG_CRUN: &'static str = "--log-level";

    /// Generate the OCI runtime CLI arguments of a create, or of a restore
    /// from the checkpoint at `restore_path`, like conmon with `--restore`:
    /// the `restore` command takes the place of `create`, with `--detach`,
    /// as the container runs once it got restored, the checkpoint as
    /// `--image-path` and the bundle as `--work-path`, where CRIU runs and
    /// writes its logs. The bundle and the PID file stay the same, and so do
    /// the `command_args`, which carry the options of the restore, like the
    /// `--runtime-opt` of conmon.
    pub(crate) fn create_args(
        self,
        bundle_path: &Path,
        restore_path: Option<&Path>,
        command_args: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Vec<String>> {
        let mut args = Vec::with_capacity(16);
        args.extend(self.default_args());

        args.push(if restore_path.is_some() {
            "restore".into()
        } else {
            "create".into()
        });
        args.push("--bundle".into());
        args.push(bundle_path.display().to_string());
        args.push("--pid-file".into());
        args.push(self.pidfile.display().to_string());

        if let Some(restore_path) = restore_path {
            args.push("--detach".into());
            args.push("--image-path".into());
            args.push(restore_path.display().to_string());
            args.push("--work-path".into());
            args.push(bundle_path.display().to_string());
        }

        args.extend(command_args.into_iter().map(|arg| arg.as_ref().into()));

        if let Some(terminal) = self.container_io.terminal() {
            args.push(format!("--console-socket={}", terminal.path().display()));
        }

        args.push(self.checked_id()?.into());

        debug!("Runtime args {args:?}");
        Ok(args)
    }

    /// Generate the OCI runtime CLI arguments of an exec, of an exec_sync or
    /// a streaming one alike. They are not logged here, the command may carry
    /// secrets, see [`LoggedArgs`].
    pub(crate) fn exec_args(
        &self,
        command: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<ExecArgs> {
        let mut args = Vec::with_capacity(12);
        args.extend(self.default_args());

        args.push("exec".into());
        args.push("-d".into());

        if let Some(terminal) = self.container_io.terminal() {
            args.push(format!("--console-socket={}", terminal.path().display()));
            args.push("--tty".into());
        }

        args.push(format!("--pid-file={}", self.pidfile.display()));
        // Ends the options of the runtime: the ID and the command follow as
        // positional arguments, also a command which starts with a dash,
        // which a runtime would take for an option of its own otherwise.
        args.push("--".into());
        args.push(self.checked_id()?.into());
        let command_start = args.len();
        args.extend(command.into_iter().map(|arg| arg.as_ref().into()));

        Ok(ExecArgs {
            args,
            command_start,
        })
    }

    /// The container ID, which the runtimes take as a positional argument.
    /// One which starts with a dash would be taken for an option.
    fn checked_id(&self) -> Result<&str> {
        if self.id.is_empty() || self.id.starts_with('-') {
            bail!("invalid container ID {:?}", self.id)
        }
        Ok(self.id)
    }

    /// Build the default arguments for any provided runtime.
    fn default_args(&self) -> Vec<String> {
        let mut args = vec![];

        if self.config.runtime_is_crun() {
            args.push(format!("--log=journald:{}", self.id));

            // crun takes no trace or info level, trace gets the most output
            // it has.
            let level = match self.config.log_level() {
                LogLevel::Trace | LogLevel::Debug => Some("debug"),
                LogLevel::Warn => Some("warning"),
                LogLevel::Error => Some("error"),
                LogLevel::Info | LogLevel::Off => None,
            };
            if let Some(level) = level {
                args.push(format!("{}={level}", Self::LOG_LEVEL_FLAG_CRUN));
            }
        }

        if let Some(rr) = self.config.runtime_root() {
            args.push(format!("--root={}", rr.display()));
        }

        if self.cgroup_manager == CgroupManager::Systemd {
            args.push(Self::SYSTEMD_CGROUP_ARG.into());
        }

        args
    }
}

/// The arguments of an exec, which end with the command of the user.
pub(crate) struct ExecArgs {
    pub(crate) args: Vec<String>,

    /// Where the command starts in `args`, see [`LoggedArgs`].
    pub(crate) command_start: usize,
}

/// The arguments of a runtime call as they get logged. Those of an exec end
/// with the command of the user, which may carry secrets, so only the number
/// of its arguments gets logged.
pub(crate) struct LoggedArgs<'a> {
    pub(crate) args: &'a [String],

    /// Where the command of an exec starts in `args`.
    pub(crate) command_start: Option<usize>,
}

impl fmt::Debug for LoggedArgs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.command_start {
            Some(start) => {
                let (args, command) = self.args.split_at(start.min(self.args.len()));
                write!(f, "{args:?} with {} command args", command.len())
            }
            None => write!(f, "{:?}", self.args),
        }
    }
}

#[cfg(test)]
mod tests;
