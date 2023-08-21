use crate::{container_io::Pipe, cri_logger::CriLogger, json_logger::JSONLogger};
use anyhow::Result;
use capnp::struct_list::Reader;
use conmon_common::conmon_capnp::conmon::log_driver::{Owned, Type};
use futures::future::join_all;
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

impl ContainerLog {
    /// Create a new default SharedContainerLog.
    pub fn new() -> SharedContainerLog {
        Arc::new(RwLock::new(Self::default()))
    }

    /// Create a new SharedContainerLog from an capnp owned reader.
    pub fn from(reader: Reader<Owned>) -> Result<SharedContainerLog> {
        let drivers = reader
            .iter()
            .flat_map(|x| -> Result<_> {
                Ok(match x.get_type()? {
                    Type::ContainerRuntimeInterface => {
                        LogDriver::ContainerRuntimeInterface(CriLogger::new(
                            x.get_path()?,
                            if x.get_max_size() > 0 {
                                Some(x.get_max_size() as usize)
                            } else {
                                None
                            },
                        )?)
                    }
                })
            })
            .collect();
        Ok(Arc::new(RwLock::new(Self { drivers })))
    }

    /// Asynchronously initialize all loggers.
    pub async fn init(&mut self) -> Result<()> {
        join_all(
            self.drivers
                .iter_mut()
                .map(|x| match x {
                    LogDriver::ContainerRuntimeInterface(ref mut cri_logger) => cri_logger.init(),
                })
                .collect::<Vec<_>>(),
        )
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
        Ok(())
    }

    /// Reopen the container logs.
    pub async fn reopen(&mut self) -> Result<()> {
        join_all(
            self.drivers
                .iter_mut()
                .map(|x| match x {
                    LogDriver::ContainerRuntimeInterface(ref mut cri_logger) => cri_logger.reopen(),
                })
                .collect::<Vec<_>>(),
        )
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
        Ok(())
    }

    /// Write the contents of the provided reader into all loggers.
    pub async fn write<T>(&mut self, pipe: Pipe, bytes: T) -> Result<()>
    where
        T: AsyncBufRead + Unpin + Copy,
    {
        join_all(
            self.drivers
                .iter_mut()
                .map(|x| match x {
                    LogDriver::ContainerRuntimeInterface(ref mut cri_logger) => {
                        cri_logger.write(pipe, bytes)
                    }
                })
                .collect::<Vec<_>>(),
        )
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
        Ok(())
    }
}
