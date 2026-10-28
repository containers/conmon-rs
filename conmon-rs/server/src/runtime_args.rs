//! The command line of the OCI runtime for a create or an exec.

use crate::{
    config::{Config, LogLevel},
    conmon_capnp::conmon::CgroupManager,
    container_io::{ContainerIO, ContainerIOType},
};
use anyhow::{Context, Result, bail};
use std::path::Path;
use tracing::debug;

pub(crate) struct GenerateRuntimeArgs<'a> {
    pub(crate) config: &'a Config,
    pub(crate) id: &'a str,
    pub(crate) container_io: &'a ContainerIO,
    pub(crate) pidfile: &'a Path,
    pub(crate) cgroup_manager: CgroupManager,
}

impl GenerateRuntimeArgs<'_> {
    const SYSTEMD_CGROUP_ARG: &'static str = "--systemd-cgroup";
    const RUNTIME_CRUN: &'static str = "crun";
    const LOG_LEVEL_FLAG_CRUN: &'static str = "--log-level";

    /// Generate the OCI runtime CLI arguments from the provided parameters.
    pub fn create_args(
        self,
        bundle_path: &Path,
        global_args: impl IntoIterator<Item = impl AsRef<str>>,
        command_args: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Vec<String>> {
        let mut args = Vec::with_capacity(16);
        args.extend(self.default_args().context("build default runtime args")?);

        args.extend(global_args.into_iter().map(|arg| arg.as_ref().into()));

        args.push("create".into());
        args.push("--bundle".into());
        args.push(bundle_path.display().to_string());
        args.push("--pid-file".into());
        args.push(self.pidfile.display().to_string());

        args.extend(command_args.into_iter().map(|arg| arg.as_ref().into()));

        if let ContainerIOType::Terminal(terminal) = self.container_io.typ() {
            args.push(format!("--console-socket={}", terminal.path().display()));
        }

        args.push(self.checked_id()?.into());

        debug!("Runtime args {:?}", args.join(" "));
        Ok(args)
    }

    /// Generate the OCI runtime CLI arguments from the provided parameters.
    pub(crate) fn exec_sync_args(
        &self,
        command: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Vec<String>> {
        let mut args = self
            .exec_sync_args_without_command()
            .context("exec sync args without command")?;

        args.extend(command.into_iter().map(|arg| arg.as_ref().into()));

        debug!("Exec args {:?}", args.join(" "));
        Ok(args)
    }

    pub(crate) fn exec_sync_args_without_command(&self) -> Result<Vec<String>> {
        let mut args = Vec::with_capacity(12);
        args.extend(self.default_args().context("build default runtime args")?);

        args.push("exec".into());
        args.push("-d".into());

        if let ContainerIOType::Terminal(terminal) = self.container_io.typ() {
            args.push(format!("--console-socket={}", terminal.path().display()));
            args.push("--tty".into());
        }

        args.push(format!("--pid-file={}", self.pidfile.display()));
        args.push(self.checked_id()?.into());

        Ok(args)
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
    fn default_args(&self) -> Result<Vec<String>> {
        let mut args = vec![];

        if self
            .config
            .runtime()
            .file_name()
            .context("no filename in path")?
            == Self::RUNTIME_CRUN
        {
            debug!("Found crun used as runtime");
            args.push(format!("--log=journald:{}", self.id));

            match self.config.log_level() {
                &LogLevel::Debug | &LogLevel::Error => args.push(format!(
                    "{}={}",
                    Self::LOG_LEVEL_FLAG_CRUN,
                    self.config.log_level()
                )),
                &LogLevel::Warn => args.push(format!("{}=warning", Self::LOG_LEVEL_FLAG_CRUN)),
                _ => {}
            }
        }

        if let Some(rr) = self.config.runtime_root() {
            args.push(format!("--root={}", rr.display()));
        }

        if self.cgroup_manager == CgroupManager::Systemd {
            args.push(Self::SYSTEMD_CGROUP_ARG.into());
        }

        Ok(args)
    }
}

#[cfg(test)]
mod tests;
