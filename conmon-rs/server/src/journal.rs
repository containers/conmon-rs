//! A `tracing` writer which forwards the server logs to journald.

use libsystemd::logging::{Priority, journal_print};
use std::{
    io::{self, Error, Write},
    str,
};
use tracing::{Level, Metadata};
use tracing_subscriber::fmt::writer::MakeWriter;

/// Writes every log line as one journal entry with the priority derived from
/// the tracing level.
#[derive(Default)]
pub struct Journal {
    priority: Option<Priority>,
}

impl Journal {
    const DEFAULT_PRIORITY: Priority = Priority::Notice;

    fn priority_for(level: &Level) -> Priority {
        match *level {
            Level::ERROR => Priority::Error,
            Level::WARN => Priority::Warning,
            Level::INFO => Priority::Info,
            Level::DEBUG | Level::TRACE => Priority::Debug,
        }
    }
}

impl Write for Journal {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let s = str::from_utf8(buf).map_err(Error::other)?;
        journal_print(self.priority.unwrap_or(Self::DEFAULT_PRIORITY), s).map_err(Error::other)?;
        Ok(s.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Journal {
    type Writer = Journal;

    fn make_writer(&'a self) -> Self::Writer {
        Journal::default()
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        Journal {
            priority: Some(Self::priority_for(meta.level())),
        }
    }
}
