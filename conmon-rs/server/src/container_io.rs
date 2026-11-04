//! The stdio of the containers and execs, pipes or a terminal, which the
//! server reads into the log drivers, the attach clients and the output of the
//! execs.

use crate::{
    attach::SharedContainerAttach, container_log::SharedContainerLog, fs_util, random_id::RandomId,
    terminal::Terminal,
};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
};
use tracing::debug;

mod drain;
mod read_loop;
mod stdio;

#[cfg(test)]
pub(crate) use drain::DRAIN_TIMEOUT;
pub(crate) use drain::{DRAIN_MAX_LOG_WAIT, DRAIN_MAX_WAIT, Drain, Lag};
pub(crate) use read_loop::{READ_BUFFER_SIZE, with_scratch};
pub(crate) use stdio::{EarlyStdio, StdioSender};
use stdio::{StdioChannel, StdioCollector};

/// How many read chunks the stdio and terminal channels buffer per pipe.
pub(crate) const STDIO_CHANNEL_SIZE: usize = 100;

/// A container IO shared by its users, like the watcher, the attach
/// endpoints and the consumer of the output of an exec.
pub(crate) type SharedContainerIO = Arc<ContainerIO>;

/// A file path which gets removed on drop.
#[derive(Debug)]
pub(crate) struct TempFile(PathBuf);

impl TempFile {
    /// The path of the file.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // A plain unlink does not need the runtime and cannot panic without one.
        // The file may be gone already: the terminal listener removes its
        // socket once it got the descriptor, and a runtime may never write a
        // pidfile.
        if let Err(e) = fs_util::remove_file(&self.0) {
            debug!("Unable to remove temp file {}: {}", self.0.display(), e);
        }
    }
}

/// The stdio of a container or exec, a terminal or pipes, and the log
/// drivers and attach endpoints its output goes to.
#[derive(Debug)]
pub(crate) struct ContainerIO {
    /// None for a process with pipes.
    terminal: Option<Terminal>,
    logger: SharedContainerLog,
    attach: SharedContainerAttach,

    /// The stdio channels, see [`StdioSender`]. A terminal merges stderr
    /// into stdout, its stderr channel ends right away.
    stdout: StdioChannel,
    stderr: StdioChannel,

    /// Whether a consumer reads the stdio channels, see [`StdioSender`].
    consumed: Arc<AtomicBool>,

    /// Collects the output of an exec while its runtime runs, see
    /// [`ContainerIO::collect_stdio`]. Taken by the consumer of the output,
    /// which shares the IO with others already.
    collector: Mutex<Option<StdioCollector>>,
}

impl ContainerIO {
    /// The terminal of the process, none with pipes.
    pub(crate) fn terminal(&self) -> Option<&Terminal> {
        self.terminal.as_ref()
    }
    /// The log drivers which get the output.
    pub(crate) fn logger(&self) -> &SharedContainerLog {
        &self.logger
    }
    /// The attach endpoints of the process.
    pub(crate) fn attach(&self) -> &SharedContainerAttach {
        &self.attach
    }

    /// Resize the terminal. Errors in case of no terminal containers.
    pub(crate) fn resize(&self, width: u16, height: u16) -> Result<()> {
        if !self.try_resize(width, height)? {
            bail!("container has no terminal")
        }
        Ok(())
    }

    /// Resize the terminal if the container has one and return whether it
    /// does. One which the runtime did not connect yet gets the size once
    /// it does, see [`TerminalResizer::resize`](crate::terminal::TerminalResizer::resize).
    pub(crate) fn try_resize(&self, width: u16, height: u16) -> Result<bool> {
        let Some(terminal) = &self.terminal else {
            return Ok(false);
        };
        terminal.resize(width, height).context("resize terminal")?;
        Ok(true)
    }

    /// The end of file character of the terminal, none without one.
    pub(crate) fn terminal_eof(&self) -> Option<u8> {
        self.terminal.as_ref().map(Terminal::eof_char)
    }
}

/// A message to be sent through the ContainerIO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Message {
    Data(Bytes, Pipe),
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Available pipe types.
pub(crate) enum Pipe {
    /// Standard output.
    StdOut,

    /// Standard error.
    StdErr,
}

impl Pipe {
    /// The name of the pipe as used in the logs.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::StdOut => "stdout",
            Self::StdErr => "stderr",
        }
    }
}

impl fmt::Display for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ContainerIO {
    /// The most output of a pipe an exec collects while its runtime runs,
    /// see [`ContainerIO::collect_stdio`], and the most an `exec_sync`
    /// reads of each pipe.
    pub(crate) const MAX_STDIO_STREAM_SIZE: usize = 16 * 1024 * 1024;

    /// Create a new container IO instance.
    pub(crate) fn new(terminal: bool, logger: SharedContainerLog) -> Result<Self> {
        let terminal = terminal
            .then(Terminal::new)
            .transpose()
            .context("create new terminal")?;
        let consumed = Arc::default();
        let stdout = StdioChannel::new(STDIO_CHANNEL_SIZE, &consumed);
        // The channel preallocates its capacity. The one of stderr carries
        // nothing with a terminal. An exec keeps it as long as the IO, only
        // its consumer closes it.
        let stderr = if terminal.is_some() {
            let mut stderr = StdioChannel::new(1, &consumed);
            stderr.tx = None;
            stderr
        } else {
            StdioChannel::new(STDIO_CHANNEL_SIZE, &consumed)
        };
        Ok(Self {
            attach: SharedContainerAttach::new(terminal.is_some()),
            terminal,
            logger,
            stdout,
            stderr,
            consumed,
            collector: Mutex::default(),
        })
    }

    /// Generate a unique temp file path without creating the file.
    fn temp_file_name(directory: Option<&Path>, prefix: &str, suffix: &str) -> PathBuf {
        let dir = directory
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let unique = RandomId::new();
        dir.join(format!("{prefix}{unique}{suffix}"))
    }

    /// A unique file path like [`ContainerIO::temp_file_name`], whose file
    /// gets removed on drop. This covers early returns and cancelled futures,
    /// which would leave for example the PID file of an exec behind.
    pub(crate) fn temp_file(directory: Option<&Path>, prefix: &str, suffix: &str) -> TempFile {
        TempFile(Self::temp_file_name(directory, prefix, suffix))
    }
}

#[cfg(test)]
mod tests;
