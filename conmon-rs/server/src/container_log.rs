use crate::cri_logger::CriLogger;
use anyhow::{Context, Result};
use capnp::struct_list::Reader;
use conmon_common::conmon_capnp::conmon::log_driver::{Owned, Type};
use std::sync::Arc;
use tokio::{io::AsyncBufRead, sync::RwLock};

pub type SharedContainerLog = Arc<RwLock<ContainerLog>>;

#[derive(Debug, Default)]
pub struct ContainerLog {
    drivers: Vec<LogDriver>,
}

#[derive(Debug)]
enum LogDriver {
    ContainerRuntimeInterface(CriLogger),
}

#[derive(Clone, Copy, Debug)]
/// Available pipe types.
pub enum Pipe {
    /// Standard output.
    StdOut,

    /// Standard error.
    StdErr,
}

impl ContainerLog {
    pub fn from(reader: Reader<Owned>) -> Result<SharedContainerLog> {
        let drivers = reader
            .iter()
            .flat_map(|x| -> Result<_> {
                Ok(match x.get_type()? {
                    Type::ContainerRuntimeInterface => {
                        LogDriver::ContainerRuntimeInterface(CriLogger::new(x.get_path()?, None)?)
                    }
                })
            })
            .collect();
        Ok(Arc::new(RwLock::new(Self { drivers })))
    }

    /// Asynchronously initialize all loggers.
    pub async fn init(&mut self) -> Result<()> {
        for logger in self.drivers.iter_mut() {
            match logger {
                LogDriver::ContainerRuntimeInterface(ref mut cri_logger) => {
                    cri_logger.init().await.context("init CRI logger")?
                }
            }
        }
        Ok(())
    }

    /// Reopen the container logs.
    pub async fn reopen(&mut self) -> Result<()> {
        for logger in self.drivers.iter_mut() {
            match logger {
                LogDriver::ContainerRuntimeInterface(ref mut cri_logger) => {
                    cri_logger.reopen().await.context("reopen CRI logs")?
                }
            }
        }
        Ok(())
    }

    /// Write the contents of the provided reader into all loggers.
    pub async fn write<T>(&mut self, pipe: Pipe, bytes: T) -> Result<()>
    where
        T: AsyncBufRead + Unpin + Copy,
    {
        for logger in self.drivers.iter_mut() {
            match logger {
                LogDriver::ContainerRuntimeInterface(ref mut cri_logger) => cri_logger
                    .write(pipe, bytes)
                    .await
                    .context("write CRI logs")?,
            }
        }
        Ok(())
    }
}
