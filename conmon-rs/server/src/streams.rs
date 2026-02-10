//! Pseudo terminal implementation.

use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, Message, Pipe},
    container_log::SharedContainerLog,
};
use anyhow::Result;
use async_channel::{Receiver, Sender};
use tokio::{
    process::{ChildStderr, ChildStdin, ChildStdout},
    task,
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error};

#[derive(Debug)]
pub struct Streams {
    logger: SharedContainerLog,
    attach: SharedContainerAttach,
    pub message_rx_stdout: Receiver<Message>,
    message_tx_stdout: Sender<Message>,
    pub message_rx_stderr: Receiver<Message>,
    message_tx_stderr: Sender<Message>,
}

impl Streams {
    pub fn logger(&self) -> &SharedContainerLog {
        &self.logger
    }
    pub fn attach(&self) -> &SharedContainerAttach {
        &self.attach
    }
    pub fn message_tx_stdout(&self) -> &Sender<Message> {
        &self.message_tx_stdout
    }
    pub fn message_tx_stderr(&self) -> &Sender<Message> {
        &self.message_tx_stderr
    }
}

impl Streams {
    /// Create a new Streams instance.
    pub fn new(logger: SharedContainerLog, attach: SharedContainerAttach) -> Result<Self> {
        debug!("Creating new IO streams");

        let (message_tx_stdout, message_rx_stdout) = async_channel::bounded(100);
        let (message_tx_stderr, message_rx_stderr) = async_channel::bounded(100);

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
        token: CancellationToken,
    ) {
        debug!("Start reading from IO streams");
        let logger = self.logger().clone();
        let attach = self.attach().clone();
        let message_tx = self.message_tx_stdout().clone();

        if let Some(stdin) = stdin {
            task::spawn(
                async move {
                    if let Err(e) = ContainerIO::read_loop_stdin(stdin, attach, token).await {
                        error!("Stdin read loop failure: {:#}", e);
                    }
                }
                .instrument(debug_span!("stdin")),
            );
        }

        let attach = self.attach().clone();
        if let Some(stdout) = stdout {
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

        let logger = self.logger().clone();
        let attach = self.attach().clone();
        let message_tx = self.message_tx_stderr().clone();
        if let Some(stderr) = stderr {
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

        let sut = Streams::new(logger, attach)?;

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

        let msg = sut
            .message_rx_stdout
            .recv()
            .await
            .context("no message on stdout")?;

        assert_eq!(msg_string(msg)?, expected);

        // There is no child_reaper instance paying attention to the child we've created,
        // so the read_loops must be cancelled here instead.
        token.cancel();

        let msg = sut
            .message_rx_stdout
            .recv()
            .await
            .context("no message on stdout")?;
        assert_eq!(msg, Message::Done);
        assert!(sut.message_rx_stdout.try_recv().is_err());

        let msg = sut
            .message_rx_stderr
            .recv()
            .await
            .context("no message on stderr")?;
        assert_eq!(msg, Message::Done);
        assert!(sut.message_rx_stderr.try_recv().is_err());

        Ok(())
    }
}
