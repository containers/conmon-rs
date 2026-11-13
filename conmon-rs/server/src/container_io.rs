//! The stdio of the containers and execs, pipes or a terminal, which the
//! server reads into the log drivers, the attach clients and the output of the
//! execs.

use crate::{attach::SharedContainerAttach, container_log::SharedContainerLog, terminal::Terminal};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use std::{
    fmt,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

mod drain;
mod read_loop;
mod stdio;

#[cfg(test)]
pub(crate) use drain::DRAIN_TIMEOUT;
pub(crate) use drain::{DRAIN_MAX_LOG_WAIT, DRAIN_MAX_WAIT, Drain, Lag};
pub(crate) use stdio::{EarlyStdio, StdioSender};
use stdio::{StdioChannel, StdioCollector};

/// How many read chunks the stdio and terminal channels buffer per pipe.
pub(crate) const STDIO_CHANNEL_SIZE: usize = 100;

/// The most a read of a container I/O stream takes at once. The read loops
/// read into the scratch buffer of their thread and copy each chunk out of
/// it, see [`with_scratch`](crate::util::with_scratch), so an idle container
/// holds no read buffer. 8 KiB halves the reads and log writes of 4 KiB for
/// bulk output, the log files start the buffer of each chunk with as much,
/// see `LogFile::BUF_CAPACITY`.
pub(crate) const READ_BUFFER_SIZE: usize = 8 * 1024;

/// How much of the end of a stderr gets kept for the error of a runtime
/// which failed, with pipes of its own or on the stderr of the process. The
/// last lines hold the reason, a runtime with debug logging writes far more.
pub(crate) const STDERR_TAIL_SIZE: usize = 64 * 1024;

/// A container IO shared by its users, like the watcher, the attach
/// endpoints and the consumer of the output of an exec.
pub(crate) type SharedContainerIO = Arc<ContainerIO>;

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

/// Available pipe types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    /// The first byte of a packet to an attach client which carries output
    /// of the pipe, see the attach socket section of docs/protocols.md.
    pub(crate) const fn attach_packet_byte(self) -> u8 {
        match self {
            Self::StdOut => 2,
            Self::StdErr => 3,
        }
    }

    /// The channel of the pipe in the remote command protocols of the
    /// streaming server, the first byte of a WebSocket message.
    pub(crate) const fn remotecommand_channel(self) -> u8 {
        match self {
            Self::StdOut => 1,
            Self::StdErr => 2,
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
        // With a terminal, stderr carries nothing: no read loop takes its
        // sender, so it goes right away and the consumers see the end. A
        // channel which never got a message holds no buffer, whatever its
        // capacity. An exec keeps the receiver as long as the IO, only its
        // consumer closes it.
        let mut stderr = StdioChannel::new(STDIO_CHANNEL_SIZE, &consumed);
        if terminal.is_some() {
            stderr.tx = None;
        }
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
}

#[cfg(test)]
mod tests;
