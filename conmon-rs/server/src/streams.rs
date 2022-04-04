//! Pseudo terminal implementation.

use crate::{container_io::Message, stream::Stream};
use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
use getset::Getters;
use log::{debug, error, trace};
use nix::{
    fcntl::OFlag,
    sys::stat::{self, Mode},
    unistd,
};
use std::{
    fs::OpenOptions,
    io::{BufReader, Read},
    os::unix::io::IntoRawFd,
    str,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

#[derive(Debug, Getters)]
#[getset(get)]
pub struct Streams {
    #[getset(get = "pub")]
    message_rx: Receiver<Message>,

    #[getset(get = "pub")]
    stop_tx: CrossbeamSender<()>,
}

impl Streams {
    /// Create a new Streams instance.
    pub fn new() -> Result<Self> {
        debug!("Creating new IO streams");
        Self::disconnect_std_streams().context("disconnect standard streams")?;

        let (stdout_fd_read, stdout_fd_write) =
            unistd::pipe2(OFlag::O_CLOEXEC).context("create stdout pipe")?;
        unistd::dup2(stdout_fd_write, libc::STDOUT_FILENO).context("dup over stdout")?;

        let (stderr_fd_read, stderr_fd_write) =
            unistd::pipe2(OFlag::O_CLOEXEC).context("create stderr pipe")?;
        unistd::dup2(stderr_fd_write, libc::STDERR_FILENO).context("dup over stderr")?;

        let mode = Mode::from_bits_truncate(0o777);
        stat::fchmod(libc::STDOUT_FILENO, mode).context("chmod stdout")?;
        stat::fchmod(libc::STDERR_FILENO, mode).context("chmod stderr")?;

        let (message_tx, message_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = unbounded();

        let stdout = stdout_fd_read.into();
        let stderr = stderr_fd_read.into();
        thread::spawn(move || Self::read_loop(message_tx, stop_rx, stdout, stderr));

        Ok(Self {
            message_rx,
            stop_tx,
        })
    }

    fn disconnect_std_streams() -> Result<()> {
        const DEV_NULL: &str = "/dev/null";

        let dev_null_r = OpenOptions::new().read(true).open(DEV_NULL)?.into_raw_fd();
        let dev_null_w = OpenOptions::new().write(true).open(DEV_NULL)?.into_raw_fd();

        unistd::dup2(dev_null_r, libc::STDIN_FILENO).context("dup over stdin")?;
        unistd::dup2(dev_null_w, libc::STDOUT_FILENO).context("dup over stdout")?;
        unistd::dup2(dev_null_w, libc::STDERR_FILENO).context("dup over stderr")?;

        Ok(())
    }

    fn read_loop(
        message_tx: Sender<Message>,
        stop_rx: CrossbeamReceiver<()>,
        stdout: Stream,
        stderr: Stream,
    ) {
        debug!("Start reading from IO streams");

        let message_tx_stdout = message_tx.clone();
        let stop_rx_clone = stop_rx.clone();

        thread::spawn(move || {
            Self::read_loop_single_stream(message_tx_stdout, stop_rx_clone, stdout)
        });
        thread::spawn(move || Self::read_loop_single_stream(message_tx, stop_rx, stderr));
    }

    fn read_loop_single_stream(
        message_tx: Sender<Message>,
        stop_rx: CrossbeamReceiver<()>,
        stream: Stream,
    ) -> Result<()> {
        trace!("Start reading from single stream: {:?}", stream);

        let message_tx_clone = message_tx.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut buf = vec![0; 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        debug!("Read {} bytes", n);
                        if let Err(e) = message_tx_clone.send(Message::Data((&buf[..n]).into())) {
                            error!("Unable to send data through message channel: {}", e);
                        }
                    }
                    Err(e) => error!("Unable to read from io stream: {}", e),
                    _ => {}
                }
            }
        });

        stop_rx.recv().context("unable to wait for stop channel")?;
        debug!("Received IO stream stop signal");
        message_tx
            .send(Message::Done)
            .context("send done message")?;

        Ok(())
    }
}
