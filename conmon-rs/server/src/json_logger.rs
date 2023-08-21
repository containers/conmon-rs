//JSON logger 
//module imports begin 
use crate::container_io::Pipe;
use anyhow::{Context, Result};
use getset::{CopyGetters, Getters, Setters};
use memchr::memchr;
use std::{
    marker::Unpin,
    path::{Path, PathBuf},
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
};
use tracing::{debug, trace};
use tz::{DateTime, TimeZone};

#[derive(Debug, CopyGetters, Getters, Setters)]

pub struct JSONLogger {
    #[getset(get)] // path to the file.
    path: PathBuf,

    #[getset(set)] // open file handle of the path.
    file: Option<BufWriter<File>>,

    #[getset(get_copy)] // maximum allowed log size in bytes.
    max_log_size: Option<usize>,

    #[getset(get_copy, set)] // current bytes written to the log file.
    bytes_written: usize,
}

impl JSONLogger{
    const ERR_UNINITIALIZED: &'static str = "logger not initialized";

    //Creation of a new instance.
    pub fn new<T: AsRef<Path>>(path: T, max_log_size: Option<usize>) -> Result<CriLogger> {
        Ok(Self {
            path: path.as_ref().into(),
            file: None,
            max_log_size,
            bytes_written: 0,
        })
    }
}
