use crate::{
    attach::Attach, container_log::SharedContainerLog, streams::Streams, terminal::Terminal,
};
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::{broadcast::Sender, mpsc::UnboundedReceiver, RwLock};

pub type SharedContainerIO = Arc<RwLock<ContainerIO>>;

#[derive(Debug)]
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
    pub async fn new(terminal: bool, logger: SharedContainerLog) -> Result<Self> {
        Ok(if terminal {
            Terminal::new(logger)
                .await
                .context("create new terminal")?
                .into()
        } else {
            Streams::new(logger).context("create new streams")?.into()
        })
    }

    /// Builds a shared reference from the current ContainerIO.
    pub fn to_shared(self) -> SharedContainerIO {
        Arc::new(RwLock::new(self))
    }

    /// Return the message receiver for the underlying type.
    pub fn receiver(&mut self) -> &mut UnboundedReceiver<Message> {
        match self {
            ContainerIO::Terminal(t) => t.message_rx_mut(),
            ContainerIO::Streams(s) => s.message_rx_mut(),
        }
    }

    /// Returns the stop channel if available.
    pub fn stop_tx(&self) -> Option<Sender<()>> {
        match self {
            ContainerIO::Terminal(_) => None,
            ContainerIO::Streams(i) => i.stop_tx().clone().into(),
        }
    }

    pub fn attach(&mut self, attach: Attach) -> Result<()> {
        match self {
            ContainerIO::Terminal(t) => {
                let stdin = attach.stdin().subscribe();
                let stdout = attach.stdout().clone();
                t.attach_tx().send((stdin, stdout))?;
            }
            ContainerIO::Streams(_i) => {} // TODO: let it work for streams
        }
        Ok(())
    }
}

/// A message to be sent through the ContainerIO.
#[derive(Debug)]
pub enum Message {
    Data(Vec<u8>),
    Done,
}
