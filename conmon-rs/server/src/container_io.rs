use crate::{container_log::SharedContainerLog, streams::Streams, terminal::Terminal};
use anyhow::{Context, Result};
use tokio::{sync::mpsc::UnboundedReceiver, time};

/// A generic abstraction over various container input-output types
pub enum ContainerIO {
    Terminal(Terminal),
    Streams(Streams),
}

impl From<Terminal> for ContainerIO {
    fn from(c: Terminal) -> Self {
        Self::Terminal(c)
    }
}

impl From<Streams> for ContainerIO {
    fn from(i: Streams) -> Self {
        Self::Streams(i)
    }
}

impl ContainerIO {
    /// Create a new container IO instance.
    pub fn new(terminal: bool, logger: SharedContainerLog) -> Result<Self> {
        Ok(if terminal {
            Terminal::new(logger).context("create new terminal")?.into()
        } else {
            Streams::new(logger).context("create new streams")?.into()
        })
    }

    pub async fn read_all_with_timeout(
        &mut self,
        time_to_timeout: Option<time::Instant>,
    ) -> (Vec<u8>, Vec<u8>, bool) {
        match self {
            ContainerIO::Terminal(t) => {
                let (stdout, timed_out) =
                    Self::read_stream_with_timeout(time_to_timeout, t.message_rx_mut()).await;
                (stdout, vec![], timed_out)
            }
            ContainerIO::Streams(s) => {
                let stdout_rx = &mut s.message_rx_stdout;
                let stderr_rx = &mut s.message_rx_stderr;
                let (stdout, stderr) = tokio::join!(
                    Self::read_stream_with_timeout(time_to_timeout, stdout_rx),
                    Self::read_stream_with_timeout(time_to_timeout, stderr_rx),
                );
                let timed_out = stdout.1 || stderr.1;
                (stdout.0, stderr.0, timed_out)
            }
        }
    }

    async fn read_stream_with_timeout(
        time_to_timeout: Option<time::Instant>,
        receiver: &mut UnboundedReceiver<Message>,
    ) -> (Vec<u8>, bool) {
        let mut stdio = vec![];
        let mut timed_out = false;
        loop {
            let msg = if let Some(time_to_timeout) = time_to_timeout {
                match time::timeout_at(time_to_timeout, receiver.recv()).await {
                    Ok(Some(msg)) => msg,
                    Err(_) => {
                        timed_out = true;
                        Message::Done
                    }
                    Ok(None) => unreachable!(),
                }
            } else {
                match receiver.recv().await {
                    Some(msg) => msg,
                    None => Message::Done,
                }
            };

            match msg {
                Message::Data(s) => stdio.extend(s),
                Message::Done => break,
            }
        }
        (stdio, timed_out)
    }
}

/// A message to be sent through the ContainerIO.
#[derive(Debug)]
pub enum Message {
    Data(Vec<u8>),
    Done,
}
