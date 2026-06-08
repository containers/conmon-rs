mod cri;
mod journald;
mod json;

use crate::{
    container_io::Pipe, container_log::cri::CriLogger, container_log::journald::JournaldLogger,
    container_log::json::JsonLogger,
};
use anyhow::Result;
use capnp::struct_list::Reader;
use conmon_common::conmon_capnp::conmon::log_driver::{Owned, Type};
use std::sync::Arc;
use tokio::sync::RwLock;

pub type SharedContainerLog = Arc<RwLock<ContainerLog>>;

#[derive(Debug, Default)]
pub struct ContainerLog {
    drivers: Vec<LogDriver>,
}

#[derive(Debug)]
enum LogDriver {
    ContainerRuntimeInterface(CriLogger),
    Journald(JournaldLogger),
    Json(JsonLogger),
}

impl ContainerLog {
    /// Create a new default SharedContainerLog.
    pub fn new() -> SharedContainerLog {
        Arc::new(RwLock::new(Self::default()))
    }

    /// Check if this logger has any drivers configured.
    pub fn has_drivers(&self) -> bool {
        !self.drivers.is_empty()
    }

    /// Create a new SharedContainerLog from an owned reader.
    pub fn from(reader: Reader<Owned>) -> Result<SharedContainerLog> {
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
        Ok(Arc::new(RwLock::new(Self { drivers })))
    }

    /// Asynchronously initialize all loggers.
    pub async fn init(&mut self) -> Result<()> {
        for driver in &mut self.drivers {
            match driver {
                LogDriver::ContainerRuntimeInterface(logger) => logger.init().await?,
                LogDriver::Json(logger) => logger.init().await?,
                LogDriver::Journald(logger) => logger.init().await?,
            }
        }
        Ok(())
    }

    /// Reopen the container logs.
    pub async fn reopen(&mut self) -> Result<()> {
        for driver in &mut self.drivers {
            match driver {
                LogDriver::ContainerRuntimeInterface(logger) => logger.reopen().await?,
                LogDriver::Json(logger) => logger.reopen().await?,
                LogDriver::Journald(logger) => logger.reopen().await?,
            }
        }
        Ok(())
    }

    /// Write the contents of the provided bytes into all loggers.
    pub async fn write(&mut self, pipe: Pipe, bytes: &[u8]) -> Result<()> {
        for driver in &mut self.drivers {
            match driver {
                LogDriver::ContainerRuntimeInterface(logger) => logger.write(pipe, bytes).await?,
                LogDriver::Journald(logger) => logger.write(pipe, bytes).await?,
                LogDriver::Json(logger) => logger.write(pipe, bytes).await?,
            }
        }
        Ok(())
    }
}
