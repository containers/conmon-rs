//! Pseudo terminal implementation.

use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, Message, Pipe},
    container_log::SharedContainerLog,
};
use anyhow::Result;
use getset::{Getters, MutGetters};
use log::debug;
use std::os::unix::io::AsRawFd;
use tokio::{
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::mpsc,
    task,
};

#[derive(Debug, Getters, MutGetters)]
#[getset(get)]
pub struct Streams {
    #[getset(get = "pub")]
    logger: SharedContainerLog,

    #[getset(get = "pub")]
    attach: SharedContainerAttach,

    #[getset(get = "pub")]
    pub message_rx_stdout: mpsc::UnboundedReceiver<Message>,

    #[getset(get = "pub")]
    message_tx_stdout: mpsc::UnboundedSender<Message>,

    #[getset(get = "pub")]
    pub message_rx_stderr: mpsc::UnboundedReceiver<Message>,

    #[getset(get = "pub")]
    message_tx_stderr: mpsc::UnboundedSender<Message>,
}

impl Streams {
    /// Create a new Streams instance.
    pub fn new(logger: SharedContainerLog, attach: SharedContainerAttach) -> Result<Self> {
        debug!("Creating new IO streams");

        let (message_tx_stdout, message_rx_stdout) = mpsc::unbounded_channel();
        let (message_tx_stderr, message_rx_stderr) = mpsc::unbounded_channel();

        Ok(Self {
            logger,
            attach,
            message_rx_stdout,
            message_tx_stdout,
            message_rx_stderr,
            message_tx_stderr,
        })
    }

    pub fn handle_stdio_receive(
        &self,
        stdin: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
    ) {
        debug!("Start reading from IO streams");
        let logger = self.logger().clone();
        let attach = self.attach().clone();
        let message_tx = self.message_tx_stdout().clone();

        if let Some(stdin) = stdin {
            task::spawn(
                async move { ContainerIO::read_loop_stdin(stdin.as_raw_fd(), attach).await },
            );
        }

        let attach = self.attach().clone();
        if let Some(stdout) = stdout {
            task::spawn(async move {
                ContainerIO::read_loop(stdout.as_raw_fd(), Pipe::StdOut, logger, message_tx, attach)
                    .await
            });
        }

        let logger = self.logger().clone();
        let attach = self.attach().clone();
        let message_tx = self.message_tx_stderr().clone();
        if let Some(stderr) = stderr {
            task::spawn(async move {
                ContainerIO::read_loop(stderr.as_raw_fd(), Pipe::StdErr, logger, message_tx, attach)
                    .await
            });
        }
    }
}
