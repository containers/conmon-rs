use crate::{container_io::Pipe, journal::Journal};
use anyhow::{Context, Result};
use std::io::Write;
use tracing::debug;

#[derive(Debug)]
pub struct JournaldLogger;

impl JournaldLogger {
    pub fn new(_: Option<usize>) -> Result<Self> {
        Ok(Self)
    }

    pub fn init(&mut self) -> Result<()> {
        debug!("Initializing journald logger");
        Ok(())
    }

    pub fn write(&mut self, _: Pipe, data: &[u8]) -> Result<()> {
        let text = String::from_utf8_lossy(data);
        for line in text.lines() {
            Journal
                .write_all(line.as_bytes())
                .context("write to journal")?;
        }
        Ok(())
    }

    pub fn reopen(&mut self) -> Result<()> {
        debug!("Reopen journald log");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_journald_logger_new() {
        JournaldLogger::new(Some(1000)).unwrap();
    }

    #[test]
    fn test_journald_logger_init() {
        let mut logger = JournaldLogger::new(Some(1000)).unwrap();
        assert!(logger.init().is_ok());
    }

    #[test]
    fn test_journald_logger_write() {
        let mut logger = JournaldLogger::new(Some(1000)).unwrap();
        logger.init().unwrap();

        assert!(logger.write(Pipe::StdOut, b"Test log message\n").is_ok());
    }

    #[test]
    fn test_journald_logger_reopen() {
        let mut logger = JournaldLogger::new(Some(1000)).unwrap();
        logger.init().unwrap();

        assert!(
            logger
                .write(Pipe::StdOut, b"Test log message before reopen\n")
                .is_ok()
        );

        assert!(logger.reopen().is_ok());

        assert!(
            logger
                .write(Pipe::StdOut, b"Test log message after reopen\n")
                .is_ok()
        );
    }
}
