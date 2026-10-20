//! Setup shared by the tests of the log drivers.

use super::{
    ContainerLog, Driver, LineBuffer, Logger, SharedContainerLog, cri::CriLogger, json::JsonLogger,
};
use anyhow::Result;
use std::{fs, sync::Arc};
use tempfile::NamedTempFile;
use tokio::sync::RwLock;

/// The timestamp of every entry a [`cri_logger`] writes.
pub(crate) const TIMESTAMP: &str = "2026-10-20T12:00:00.123456789+02:00";

/// A CRI logger writing to a new temporary file, with a frozen timestamp.
pub(crate) async fn cri_logger(max_size: Option<usize>) -> Result<(NamedTempFile, CriLogger)> {
    let file = NamedTempFile::new()?;
    let mut logger = CriLogger::new(file.path(), max_size);
    logger.freeze_timestamp(TIMESTAMP);
    logger.init().await?;
    Ok((file, logger))
}

/// A JSON logger writing to a new temporary file.
pub(crate) async fn json_logger(max_size: Option<usize>) -> Result<(NamedTempFile, JsonLogger)> {
    let file = NamedTempFile::new()?;
    let mut logger = JsonLogger::new(file.path(), max_size);
    logger.init().await?;
    Ok((file, logger))
}

/// The content of a log file.
pub(crate) fn read(file: &NamedTempFile) -> Result<String> {
    Ok(fs::read_to_string(file.path())?)
}

/// A container log with a CRI driver, with a frozen timestamp, and a JSON
/// driver, writing to new temporary files. Without `init` the drivers are
/// not opened and every write fails.
pub(crate) async fn container_log(
    init: bool,
) -> Result<(NamedTempFile, NamedTempFile, SharedContainerLog)> {
    let (cri_file, json_file) = (NamedTempFile::new()?, NamedTempFile::new()?);
    let mut cri = CriLogger::new(cri_file.path(), None);
    cri.freeze_timestamp(TIMESTAMP);
    let mut log = ContainerLog {
        drivers: vec![
            Driver::Cri(cri),
            Driver::Json(JsonLogger::new(json_file.path(), None)),
        ],
        lines: LineBuffer::default(),
        buffers_lines: true,
    };
    if init {
        log.init().await?;
    }
    Ok((cri_file, json_file, Arc::new(RwLock::new(log))))
}
