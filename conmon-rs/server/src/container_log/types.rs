use crate::{
    container_io::Pipe, container_log::cri_logger::CriLogger,
    container_log::journald::JournaldLogger, container_log::json_logger::JsonLogger,
};
use anyhow::Result;
use capnp::struct_list::Reader;
use conmon_common::conmon_capnp::conmon::log_driver::{Owned, Type};
use futures::{future::join_all, FutureExt};
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
    Journald(JournaldLogger),
    Json(JsonLogger),
}

impl ContainerLog {
    ///Create a new default SharedContainerLog
    pub fn new() -> SharedContainerLog {
        Arc::new(RwLock::new(Self::default()))
    }
    /// Create a new SharedContainerLog from an capnp owned reader.
    pub fn from(reader: Reader<Owned>) -> Result<SharedContainerLog> {
        let drivers = reader
            .iter()
            .map(|x| -> Result<_> {
                match x.get_type()? {
                    Type::ContainerRuntimeInterface => {
                        Ok(Some(LogDriver::ContainerRuntimeInterface(CriLogger::new(
                            x.get_path()?,
                            if x.get_max_size() > 0 {
                                Some(x.get_max_size() as usize)
                            } else {
                                None
                            },
                        )?)))
                    }
                    Type::Json => Ok(Some(LogDriver::Json(JsonLogger::new(
                        x.get_path()?,
                        if x.get_max_size() > 0 {
                            Some(x.get_max_size() as usize)
                        } else {
                            None
                        },
                    )?))),
                    Type::Journald => Ok(Some(LogDriver::Journald(JournaldLogger::new(
                        if x.get_max_size() > 0 {
                            Some(x.get_max_size() as usize)
                        } else {
                            None
                        },
                    )?))),
                }
            })
            .filter_map(Result::transpose)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Arc::new(RwLock::new(Self { drivers })))
    }

    /// Asynchronously initialize all loggers.
    pub async fn init(&mut self) -> Result<()> {
        join_all(
            self.drivers
                .iter_mut()
                .map(|x| match x {
                    LogDriver::ContainerRuntimeInterface(ref mut cri_logger) => {
                        cri_logger.init().boxed()
                    }
                    LogDriver::Json(ref mut json_logger) => json_logger.init().boxed(),
                    LogDriver::Journald(ref mut journald_logger) => journald_logger.init().boxed(),
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
                    LogDriver::ContainerRuntimeInterface(ref mut cri_logger) => {
                        cri_logger.reopen().boxed()
                    }
                    LogDriver::Json(ref mut json_logger) => json_logger.reopen().boxed(),
                    LogDriver::Journald(ref mut journald_logger) => {
                        journald_logger.reopen().boxed()
                    }
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
        T: AsyncBufRead + Unpin + Clone, // Using Clone to satisfy both Clone and Copy requirements
    {
        let futures = self
            .drivers
            .iter_mut()
            .map(|x| {
                async fn box_future<'a, T: AsyncBufRead + Unpin + Clone>(
                    logger: &mut LogDriver,
                    pipe: Pipe,
                    bytes: T,
                ) -> Result<()> {
                    match logger {
                        LogDriver::ContainerRuntimeInterface(cri_logger) => {
                            cri_logger.write(pipe, bytes.clone()).await
                        }
                        LogDriver::Journald(journald_logger) => {
                            journald_logger.write(pipe, bytes.clone()).await
                        }
                        LogDriver::Json(json_logger) => {
                            json_logger.write(pipe, bytes.clone()).await
                        }
                    }
                }
                box_future(x, pipe, bytes.clone())
            })
            .collect::<Vec<_>>();
        join_all(futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        Ok(())
    }
}
