use crate::{container_log::SharedContainerLog, streams::Streams, terminal::Terminal};
use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedReceiver;

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

    /// Return the message receiver for the underlying type.
    pub fn receiver(&mut self) -> &mut UnboundedReceiver<Message> {
        match self {
            ContainerIO::Terminal(t) => t.message_rx_mut(),
            ContainerIO::Streams(s) => s.message_rx_mut(),
        }
    }
}

/// A message to be sent through the ContainerIO.
#[derive(Debug)]
pub enum Message {
    Data(Vec<u8>),
    Done,
}
