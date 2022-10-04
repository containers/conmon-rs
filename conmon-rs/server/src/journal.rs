use libsystemd::logging::{journal_print, Priority};
use std::{
    io::{self, Error, ErrorKind, Write},
    str,
};
use tracing_subscriber::fmt::writer::MakeWriter;

macro_rules! io_err {
    ($x:expr) => {
        $x.map_err(|e| Error::new(ErrorKind::Other, e.to_string()))?
    };
}

#[derive(Default)]
pub struct Journal;

impl Write for Journal {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let s = io_err!(str::from_utf8(buf));
        io_err!(journal_print(Priority::Notice, s));
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
}
