//! Pseudo terminal implementation.

use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, Message, Pipe},
    container_log::SharedContainerLog,
};
use anyhow::{Result, bail};
use async_channel::{Receiver, Sender};
use tokio::{
    process::{ChildStderr, ChildStdin, ChildStdout},
    task,
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error};

/// Capacity of the stdout and stderr channels in chunks.
const STDIO_CHANNEL_SIZE: usize = 100;

#[derive(Debug)]
pub struct Streams {
    logger: SharedContainerLog,
    attach: SharedContainerAttach,

    /// The stdio channels. The senders move into the read loops when the
    /// streams get attached, the receivers stay here until they are either
    /// cloned by a consumer or dropped via `drop_stdio`.
    message_rx_stdout: Option<Receiver<Message>>,
    message_tx_stdout: Option<Sender<Message>>,
    message_rx_stderr: Option<Receiver<Message>>,
    message_tx_stderr: Option<Sender<Message>>,
}

impl Streams {
    pub fn logger(&self) -> &SharedContainerLog {
        &self.logger
    }
    pub fn attach(&self) -> &SharedContainerAttach {
        &self.attach
    }
}

impl Streams {
    /// Create a new Streams instance.
    pub fn new(logger: SharedContainerLog, attach: SharedContainerAttach) -> Result<Self> {
        debug!("Creating new IO streams");

        let (message_tx_stdout, message_rx_stdout) = async_channel::bounded(STDIO_CHANNEL_SIZE);
        let (message_tx_stderr, message_rx_stderr) = async_channel::bounded(STDIO_CHANNEL_SIZE);

        Ok(Self {
            logger,
            attach,
            message_rx_stdout: Some(message_rx_stdout),
            message_tx_stdout: Some(message_tx_stdout),
            message_rx_stderr: Some(message_rx_stderr),
            message_tx_stderr: Some(message_tx_stderr),
        })
    }

    /// Retrieve clones of the stdout and stderr receivers.
    pub fn stdio(&self) -> Result<(Receiver<Message>, Receiver<Message>)> {
        match (&self.message_rx_stdout, &self.message_rx_stderr) {
            (Some(stdout), Some(stderr)) => Ok((stdout.clone(), stderr.clone())),
            _ => bail!("stdio channels have been dropped"),
        }
    }

    /// Retrieve the stdout and stderr receivers for reading.
    pub fn stdio_mut(&mut self) -> Result<(&mut Receiver<Message>, &mut Receiver<Message>)> {
        match (&mut self.message_rx_stdout, &mut self.message_rx_stderr) {
            (Some(stdout), Some(stderr)) => Ok((stdout, stderr)),
            _ => bail!("stdio channels have been dropped"),
        }
    }

    /// Drop the stdio receivers. The read loops notice the closed channels on
    /// their next send and drop their senders too, which frees the buffers.
    pub fn drop_stdio(&mut self) {
        self.message_rx_stdout = None;
        self.message_rx_stderr = None;
    }

    pub fn handle_stdio_receive(
        &mut self,
        stdin: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        token: CancellationToken,
    ) {
        debug!("Start reading from IO streams");

        if let Some(stdin) = stdin {
            let attach = self.attach().clone();
            task::spawn(
                async move {
                    if let Err(e) = ContainerIO::read_loop_stdin(stdin, attach, token).await {
                        error!("Stdin read loop failure: {:#}", e);
                    }
                }
                .instrument(debug_span!("stdin")),
            );
        }

        if let Some(stdout) = stdout {
            let logger = self.logger().clone();
            let attach = self.attach().clone();
            let message_tx = self.message_tx_stdout.take();
            task::spawn(
                async move {
                    if let Err(e) =
                        ContainerIO::read_loop(stdout, Pipe::StdOut, logger, message_tx, attach)
                            .await
                    {
                        error!("Stdout read loop failure: {:#}", e);
                    }
                }
                .instrument(debug_span!("stdout")),
            );
        }

        if let Some(stderr) = stderr {
            let logger = self.logger().clone();
            let attach = self.attach().clone();
            let message_tx = self.message_tx_stderr.take();
            task::spawn(
                async move {
                    if let Err(e) =
                        ContainerIO::read_loop(stderr, Pipe::StdErr, logger, message_tx, attach)
                            .await
                    {
                        error!("Stderr read loop failure: {:#}", e);
                    }
                }
                .instrument(debug_span!("stderr")),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_log::ContainerLog;
    use anyhow::{Context, bail};
    use std::{process::Stdio, str::from_utf8};
    use tokio::process::Command;

    fn msg_string(message: Message) -> Result<String> {
        match message {
            Message::Data(v, _) => Ok(from_utf8(&v)?.into()),
            _ => bail!("no data in message"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_success() -> Result<()> {
        let logger = ContainerLog::new();
        let attach = SharedContainerAttach::default();
        let token = CancellationToken::new();

        let mut sut = Streams::new(logger, attach)?;

        let expected = "hello world";
        let mut child = Command::new("echo")
            .arg("-n")
            .arg(expected)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        sut.handle_stdio_receive(
            child.stdin.take(),
            child.stdout.take(),
            child.stderr.take(),
            token.clone(),
        );

        let (stdout_rx, stderr_rx) = sut.stdio()?;

        let msg = stdout_rx.recv().await.context("no message on stdout")?;

        assert_eq!(msg_string(msg)?, expected);

        // There is no child_reaper instance paying attention to the child we've created,
        // so the read_loops must be cancelled here instead.
        token.cancel();

        let msg = stdout_rx.recv().await.context("no message on stdout")?;
        assert_eq!(msg, Message::Done);
        assert!(stdout_rx.try_recv().is_err());

        let msg = stderr_rx.recv().await.context("no message on stderr")?;
        assert_eq!(msg, Message::Done);
        assert!(stderr_rx.try_recv().is_err());

        Ok(())
    }
}
