mod cri;
mod journald;
mod json;

use crate::{
    container_io::Pipe, container_log::cri::CriLogger, container_log::journald::JournaldLogger,
    container_log::json::JsonLogger,
};
use anyhow::{Context, Result};
use capnp::struct_list::Reader;
use conmon_common::conmon_capnp::conmon::log_driver::{Owned, Type};
use std::sync::Arc;
use tokio::task;
use tracing::error;

/// Messages sent to the log writer task.
enum LogMessage {
    /// Log data from a pipe.
    Data(Pipe, Arc<[u8]>),
    /// Reopen all log files (for log rotation).
    Reopen(tokio::sync::oneshot::Sender<Result<()>>),
}

impl std::fmt::Debug for LogMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Data(pipe, data) => f
                .debug_tuple("Data")
                .field(pipe)
                .field(&data.len())
                .finish(),
            Self::Reopen(_) => f.debug_tuple("Reopen").finish(),
        }
    }
}

/// A channel-based handle to the log writer task.
#[derive(Clone, Debug, Default)]
pub struct SharedContainerLog {
    sender: Option<async_channel::Sender<LogMessage>>,
}

#[derive(Debug, Default)]
struct ContainerLog {
    drivers: Vec<LogDriver>,
}

#[derive(Debug)]
enum LogDriver {
    ContainerRuntimeInterface(CriLogger),
    Journald(JournaldLogger),
    Json(JsonLogger),
}

impl SharedContainerLog {
    /// Create a new SharedContainerLog with no drivers (no-op logger).
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new SharedContainerLog from log driver configuration.
    /// Parses drivers, opens log files, and spawns the writer task.
    pub fn from(reader: Reader<Owned>) -> Result<Self> {
        let drivers = reader
            .iter()
            .map(|x| -> Result<_> {
                match x.get_type()? {
                    Type::ContainerRuntimeInterface => {
                        Ok(LogDriver::ContainerRuntimeInterface(CriLogger::new(
                            x.get_path()?.to_str()?,
                            if x.get_max_size() > 0 {
                                Some(x.get_max_size() as usize)
                            } else {
                                None
                            },
                        )?))
                    }
                    Type::Json => Ok(LogDriver::Json(JsonLogger::new(
                        x.get_path()?.to_str()?,
                        if x.get_max_size() > 0 {
                            Some(x.get_max_size() as usize)
                        } else {
                            None
                        },
                    )?)),
                    Type::Journald => Ok(LogDriver::Journald(JournaldLogger::new(
                        if x.get_max_size() > 0 {
                            Some(x.get_max_size() as usize)
                        } else {
                            None
                        },
                    )?)),
                }
            })
            .collect::<Result<Vec<_>>>()?;

        if drivers.is_empty() {
            return Ok(Self::new());
        }

        let mut log = ContainerLog { drivers };
        log.init()?;

        let (sender, receiver) = async_channel::bounded(256);
        task::spawn(Self::writer_loop(log, receiver));

        Ok(Self {
            sender: Some(sender),
        })
    }

    /// Send log data to the writer task.
    pub async fn send(&self, pipe: Pipe, data: Arc<[u8]>) -> Result<()> {
        if let Some(sender) = &self.sender {
            sender
                .send(LogMessage::Data(pipe, data))
                .await
                .context("send log data to writer")?;
        }
        Ok(())
    }

    /// Request the writer task to reopen all log files.
    pub async fn reopen(&self) -> Result<()> {
        if let Some(sender) = &self.sender {
            let (tx, rx) = tokio::sync::oneshot::channel();
            sender
                .send(LogMessage::Reopen(tx))
                .await
                .context("send reopen message to writer")?;
            rx.await.context("receive reopen result")??;
        }
        Ok(())
    }

    async fn writer_loop(mut log: ContainerLog, receiver: async_channel::Receiver<LogMessage>) {
        while let Ok(msg) = receiver.recv().await {
            match msg {
                LogMessage::Data(pipe, data) => {
                    if let Err(e) = log.write(pipe, &data) {
                        error!("Log write error: {:#}", e);
                    }
                }
                LogMessage::Reopen(reply) => {
                    let _ = reply.send(log.reopen());
                }
            }
        }
        if let Err(e) = log.flush() {
            error!("Log flush error on shutdown: {:#}", e);
        }
    }
}

impl ContainerLog {
    /// Initialize all loggers.
    fn init(&mut self) -> Result<()> {
        for driver in &mut self.drivers {
            match driver {
                LogDriver::ContainerRuntimeInterface(logger) => logger.init()?,
                LogDriver::Json(logger) => logger.init()?,
                LogDriver::Journald(logger) => logger.init()?,
            }
        }
        Ok(())
    }

    /// Reopen the container logs.
    fn reopen(&mut self) -> Result<()> {
        for driver in &mut self.drivers {
            match driver {
                LogDriver::ContainerRuntimeInterface(logger) => logger.reopen()?,
                LogDriver::Json(logger) => logger.reopen()?,
                LogDriver::Journald(logger) => logger.reopen()?,
            }
        }
        Ok(())
    }

    /// Write data to all log drivers.
    fn write(&mut self, pipe: Pipe, data: &[u8]) -> Result<()> {
        for driver in &mut self.drivers {
            match driver {
                LogDriver::ContainerRuntimeInterface(logger) => logger.write(pipe, data)?,
                LogDriver::Journald(logger) => logger.write(pipe, data)?,
                LogDriver::Json(logger) => logger.write(pipe, data)?,
            }
        }
        Ok(())
    }

    /// Flush all file-based log drivers.
    fn flush(&mut self) -> Result<()> {
        for driver in &mut self.drivers {
            match driver {
                LogDriver::ContainerRuntimeInterface(logger) => logger.flush()?,
                LogDriver::Json(logger) => logger.flush()?,
                LogDriver::Journald(_) => {}
            }
        }
        Ok(())
    }
}
