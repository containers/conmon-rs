use crate::{cri_logger::CriLogger, streams::Streams, terminal::Terminal};
use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use std::{path::Path, sync::mpsc::Receiver};

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
    pub async fn new<T: AsRef<Path>>(terminal: bool, log_path: T) -> Result<Self> {
        let logger = CriLogger::from(&log_path, None)
            .await
            .context("create CRI logger")?;
        Ok(if terminal {
            Terminal::new(logger).context("create new terminal")?.into()
        } else {
            Streams::new(logger).context("create new streams")?.into()
        })
    }

    /// Return the message receiver for the underlying type.
    pub fn receiver(&self) -> &Receiver<Message> {
        match self {
            ContainerIO::Terminal(t) => t.message_rx(),
            ContainerIO::Streams(s) => s.message_rx(),
        }
    }

    /// Returns the stop channel if available.
    pub fn stop_tx(&self) -> Option<Sender<()>> {
        match self {
            ContainerIO::Terminal(_) => None,
            ContainerIO::Streams(i) => i.stop_tx().clone().into(),
        }
    }
}

/// A message to be sent through the ContainerIO.
#[derive(Debug)]
pub enum Message {
    Data(Vec<u8>),
    Done,
}
