//! Relay of sd_notify messages from a container to the host.
//!
//! The server binds a datagram socket at `<bundle>/notify/notify.sock` before
//! it runs the runtime, so that the container engine can bind mount the
//! `notify` directory into the container and point its `NOTIFY_SOCKET` at it,
//! the same way podman does with the `--sdnotify-socket` option of conmon.
//! Every datagram the container sends there is relayed to the host socket,
//! usually the one of systemd, until the container exits.
//!
//! systemd attributes the messages to the server, so only the assignments
//! which cannot affect anything outside of the container pass, the same ones
//! conmon passes. `MAINPID=` would for example make systemd manage an
//! arbitrary host process.

use crate::{
    fs_util::{self, FileId, Removal, RemoveError},
    listener,
    random_id::RandomId,
};
use anyhow::{Context, Result, bail};
use nix::sys::socket::{MsgFlags, UnixAddr, connect, recv, send};
use std::{
    fs::{self, File, Metadata, Permissions},
    os::unix::{fs::PermissionsExt, io::AsRawFd, net::UnixDatagram as StdUnixDatagram},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::{net::UnixDatagram, select, task};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, trace, warn};

#[derive(Debug)]
pub struct NotifySocket {
    /// The container side.
    socket: UnixDatagram,

    /// The host side, connected to the host socket for every message.
    host: UnixDatagram,
    host_addr: UnixAddr,

    /// Keeps the parent directory of a host socket path open, the address
    /// refers to it.
    _host_dir: Option<File>,

    /// The path of the container side and the identity of the socket bound
    /// there, so that only that one gets removed again.
    path: PathBuf,
    file: FileId,

    /// Whether relaying the last message failed. The container controls how
    /// many it sends, and while the host socket is gone they all fail, so
    /// only the first failure after a relayed message logs a warning.
    relay_failed: AtomicBool,
}

impl NotifySocket {
    /// The directory below the bundle path which holds the socket.
    pub const DIR: &'static str = "notify";

    /// The socket name inside of `DIR`.
    pub const FILE: &'static str = "notify.sock";

    /// The start of the socket name in the bundle directory before it moves
    /// into `DIR`, followed by a random ID, so that two creates with the same
    /// bundle do not use the same one.
    const TMP_PREFIX: &'static str = ".notify.sock.";

    /// systemd rejects larger notification messages (`NOTIFY_BUFFER_MAX`).
    const MAX_MESSAGE_SIZE: usize = 4096;

    /// How many queued messages get relayed after the container exited.
    const DRAIN_MAX_MESSAGES: usize = 64;

    /// The assignments which pass as they are.
    const PASS_LINES: [&'static [u8]; 5] = [
        b"READY=1",
        b"RELOADING=1",
        b"STOPPING=1",
        b"WATCHDOG=1",
        b"WATCHDOG=trigger",
    ];

    /// The assignments which pass with any value.
    const PASS_PREFIXES: [&'static [u8]; 4] =
        [b"STATUS=", b"ERRNO=", b"BUSERROR=", b"MONOTONIC_USEC="];

    /// Bind the container side socket below the bundle path, the messages
    /// get relayed to the `host` socket. An `@` prefix denotes an abstract
    /// host socket, like for `NOTIFY_SOCKET`.
    pub fn bind(bundle_path: &Path, host: &str) -> Result<Self> {
        let (host, host_addr, host_dir) =
            Self::connect_host(host).context("connect host notify socket")?;

        let path = bundle_path.join(Self::DIR).join(Self::FILE);

        // The bundle path may exceed what a socket address can hold, so bind
        // through the parent directory, which also creates it. Keep the
        // directory open until the socket file is set up.
        let (short_path, _dir) =
            listener::shorten_socket_path(&path).context("shorten notify socket path")?;

        // A leftover of a previous run makes the bind fail, but another
        // container of this bundle may still use the socket.
        if StdUnixDatagram::unbound()?.connect(&short_path).is_ok() {
            bail!("notify socket {} is in use", path.display())
        }
        fs_util::remove_file(&short_path)
            .with_context(|| format!("remove stale {}", path.display()))?;

        let (socket, metadata) = Self::bind_in_place(bundle_path, &short_path)
            .with_context(|| format!("bind notify socket {}", path.display()))?;

        debug!("Relaying notify messages from {}", path.display());
        Ok(Self {
            socket,
            host,
            host_addr,
            _host_dir: host_dir,
            path,
            file: FileId::new(&metadata),
            relay_failed: AtomicBool::new(false),
        })
    }

    /// Bind the socket in the bundle directory, open it up for the container
    /// process, which may run as any user, and move it to `short_path`.
    /// Unlike `DIR`, the container cannot reach the bundle directory, so it
    /// cannot swap the socket for a symlink between the bind and the chmod,
    /// which follows symlinks. The rename replaces whatever got placed at
    /// the path meanwhile without following it either.
    fn bind_in_place(bundle_path: &Path, short_path: &Path) -> Result<(UnixDatagram, Metadata)> {
        let name = format!("{}{}", Self::TMP_PREFIX, RandomId::new());
        let (tmp, _dir) =
            listener::shorten_path(&bundle_path.join(name)).context("shorten temporary path")?;
        let socket = UnixDatagram::bind(&tmp).context("bind")?;
        let res = fs::set_permissions(&tmp, Permissions::from_mode(0o777))
            .context("set permissions")
            .and_then(|()| fs::symlink_metadata(&tmp).context("stat"))
            .and_then(|metadata| {
                fs::rename(&tmp, short_path).context("rename")?;
                Ok(metadata)
            });
        match res {
            Ok(metadata) => Ok((socket, metadata)),
            Err(e) => {
                if let Err(e) = fs::remove_file(&tmp) {
                    debug!("Unable to remove temporary notify socket: {e}");
                }
                Err(e)
            }
        }
    }

    /// Connect a socket to the host one, so that a wrong path fails the
    /// create instead of every message.
    fn connect_host(host: &str) -> Result<(UnixDatagram, UnixAddr, Option<File>)> {
        let (addr, dir) = if let Some(name) = host.strip_prefix('@') {
            (UnixAddr::new_abstract(name.as_bytes())?, None)
        } else {
            // The path may exceed what a socket address can hold, so connect
            // through its parent directory, which resolves the current socket
            // for every connect.
            let (short_path, dir) =
                listener::shorten_path(Path::new(host)).context("shorten host socket path")?;
            (UnixAddr::new(&short_path)?, Some(dir))
        };

        let socket = UnixDatagram::unbound().context("create socket")?;
        connect(socket.as_raw_fd(), &addr).with_context(|| format!("connect {host}"))?;
        Ok((socket, addr, dir))
    }

    /// Relay messages until the token gets cancelled, which happens when the
    /// container exits.
    pub fn spawn(self, token: CancellationToken) {
        task::spawn(self.relay(token).instrument(debug_span!("notify")));
    }

    async fn relay(self, token: CancellationToken) {
        // One byte more tells a message which got cut off.
        let mut buf = vec![0; Self::MAX_MESSAGE_SIZE + 1];

        // A message received but not relayed when the container exited.
        let mut pending = None;
        loop {
            select! {
                // Prefer the messages, they are read until the socket is
                // empty after the cancellation anyway.
                biased;
                res = self.socket.recv(&mut buf) => match res {
                    Ok(len) => {
                        // The host may stop reading, do not wait for it
                        // beyond the lifetime of the container.
                        select! {
                            biased;
                            () = self.forward(&buf[..len]) => {}
                            () = token.cancelled() => {
                                pending = Some(len);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Unable to receive notify message: {e}");
                        break;
                    }
                },
                () = token.cancelled() => break,
            }
        }

        // What the container sent right before it exited. The cancellation
        // may come before the I/O driver saw the message, so do not rely on
        // the readiness tokio tracks and ask the kernel instead. A process
        // left behind could keep the socket full, so stop at some point.
        let flags = MsgFlags::MSG_DONTWAIT;
        let mut remaining = Self::DRAIN_MAX_MESSAGES;
        if let Some(len) = pending {
            self.forward_now(&buf[..len]);
            remaining -= 1;
        }
        while remaining > 0
            && let Ok(len) = recv(self.socket.as_raw_fd(), &mut buf, flags)
        {
            self.forward_now(&buf[..len]);
            remaining -= 1;
        }
        debug!("Stopped relaying notify messages");
    }

    async fn forward(&self, message: &[u8]) {
        let Some(message) = Self::check_message(message) else {
            return;
        };
        let result = match self.connect_host_again() {
            Ok(()) => self.host.send(&message).await.map(drop).context("send"),
            Err(e) => Err(e),
        };
        self.relayed(result);
    }

    /// Relay without waiting for the host, drop the message if it is full.
    fn forward_now(&self, message: &[u8]) {
        let Some(message) = Self::check_message(message) else {
            return;
        };
        let flags = MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL;
        self.relayed(
            self.connect_host_again()
                .and_then(|()| send(self.host.as_raw_fd(), &message, flags).context("send"))
                .map(drop),
        );
    }

    /// Log the failure to relay a message, as a warning only if the last one
    /// got relayed, see [`NotifySocket::relay_failed`].
    fn relayed(&self, result: Result<()>) {
        match result {
            Ok(()) => self.relay_failed.store(false, Ordering::Relaxed),
            Err(e) if !self.relay_failed.swap(true, Ordering::Relaxed) => {
                warn!("Unable to relay notify message: {e:#}");
            }
            Err(e) => debug!("Unable to relay notify message: {e:#}"),
        }
    }

    /// The host may have recreated its socket at the same path meanwhile,
    /// which a connected socket would not notice.
    fn connect_host_again(&self) -> Result<()> {
        connect(self.host.as_raw_fd(), &self.host_addr).context("connect host socket")
    }

    /// Returns the part of the message to relay, if any. A message which
    /// got cut off may miss its ready state or end in the middle of an
    /// assignment, drop it instead.
    fn check_message(message: &[u8]) -> Option<Vec<u8>> {
        // The container may send any number of them, like the assignments
        // below, only the debug log tells.
        if message.len() > Self::MAX_MESSAGE_SIZE {
            debug!(
                "Dropping notify message exceeding {} bytes",
                Self::MAX_MESSAGE_SIZE
            );
            return None;
        }
        let filtered = Self::filter_message(message);
        if filtered.len() != message.len() {
            debug!(
                "Dropping notify assignments from {:?}",
                String::from_utf8_lossy(message)
            );
        }
        if filtered.is_empty() {
            return None;
        }
        trace!(
            "Relaying notify message: {:?}",
            String::from_utf8_lossy(&filtered)
        );
        Some(filtered)
    }

    /// Keep only the allowed assignments, newline separated.
    fn filter_message(message: &[u8]) -> Vec<u8> {
        let mut filtered = Vec::with_capacity(message.len());
        for line in message.split(|&b| b == b'\n' || b == b'\r') {
            if Self::PASS_LINES.contains(&line)
                || Self::PASS_PREFIXES.iter().any(|p| line.starts_with(p))
            {
                if !filtered.is_empty() {
                    filtered.push(b'\n');
                }
                filtered.extend_from_slice(line);
            }
        }
        filtered
    }
}

impl Drop for NotifySocket {
    fn drop(&mut self) {
        // The bundle usually gets removed anyway, this just does not leave a
        // dead socket behind in case it stays, and only the own one.
        match self.file.remove_at(&self.path) {
            Ok(Removal::Removed | Removal::Missing) => {}
            Ok(Removal::Replaced) => debug!("Notify socket got replaced, keeping it"),
            Err(RemoveError::Stat(e)) => debug!("Unable to stat notify socket: {e}"),
            Err(RemoveError::Remove(e)) => debug!("Unable to remove notify socket: {e}"),
        }
    }
}

/// The entry point of the fuzz target.
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzzing;

#[cfg(test)]
mod tests;
