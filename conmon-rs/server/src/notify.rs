//! Relay of sd_notify messages from a container to the host.
//!
//! The server binds a datagram socket at `<bundle>/notify/notify.sock` before
//! it runs the runtime, so that the container engine can bind mount the
//! `notify` directory into the container and point its `NOTIFY_SOCKET` at it,
//! the same way podman does with the `--sdnotify-socket` option of conmon.
//! Every datagram the container sends there is relayed unchanged to the host
//! socket, usually the one of systemd, until the container exits.

use crate::listener::{self, DefaultListener, Listener};
use anyhow::{Context, Result, bail};
use nix::sys::socket::{MsgFlags, UnixAddr, connect, recv, send};
use std::{
    fs::{self, File, Permissions},
    io::ErrorKind,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        io::AsRawFd,
        net::UnixDatagram as StdUnixDatagram,
    },
    path::{Path, PathBuf},
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
    dev: u64,
    ino: u64,
}

impl NotifySocket {
    /// The directory below the bundle path which holds the socket.
    pub const DIR: &'static str = "notify";

    /// The socket name inside of `DIR`.
    pub const FILE: &'static str = "notify.sock";

    /// systemd rejects larger notification messages (`NOTIFY_BUFFER_MAX`).
    const MAX_MESSAGE_SIZE: usize = 4096;

    /// How many queued messages get relayed after the container exited.
    const DRAIN_MAX_MESSAGES: usize = 64;

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
        let (short_path, _dir) = Listener::<DefaultListener>::default()
            .shorten_socket_path(&path)
            .context("shorten notify socket path")?;

        // A leftover of a previous run makes the bind fail, but another
        // container of this bundle may still use the socket.
        if StdUnixDatagram::unbound()?.connect(&short_path).is_ok() {
            bail!("notify socket {} is in use", path.display())
        }
        if let Err(e) = fs::remove_file(&short_path)
            && e.kind() != ErrorKind::NotFound
        {
            return Err(e).with_context(|| format!("remove stale {}", path.display()));
        }

        let socket = UnixDatagram::bind(&short_path)
            .with_context(|| format!("bind notify socket {}", path.display()))?;

        // The container process may run as any user.
        fs::set_permissions(&short_path, Permissions::from_mode(0o777))
            .context("set notify socket permissions")?;
        let metadata = fs::metadata(&short_path).context("stat notify socket")?;

        debug!("Relaying notify messages from {}", path.display());
        Ok(Self {
            socket,
            host,
            host_addr,
            _host_dir: host_dir,
            path,
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
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
        if !Self::check_message(message) {
            return;
        }
        if let Err(e) = self.connect_host_again() {
            warn!("Unable to relay notify message: {:#}", e);
            return;
        }
        if let Err(e) = self.host.send(message).await {
            warn!("Unable to relay notify message: {e}");
        }
    }

    /// Relay without waiting for the host, drop the message if it is full.
    fn forward_now(&self, message: &[u8]) {
        if !Self::check_message(message) {
            return;
        }
        let flags = MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL;
        if let Err(e) = self
            .connect_host_again()
            .and_then(|()| send(self.host.as_raw_fd(), message, flags).context("send"))
        {
            warn!("Unable to relay notify message: {:#}", e);
        }
    }

    /// The host may have recreated its socket at the same path meanwhile,
    /// which a connected socket would not notice.
    fn connect_host_again(&self) -> Result<()> {
        connect(self.host.as_raw_fd(), &self.host_addr).context("connect host socket")
    }

    /// A message which got cut off may miss its ready state or end in the
    /// middle of an assignment, drop it instead.
    fn check_message(message: &[u8]) -> bool {
        if message.len() > Self::MAX_MESSAGE_SIZE {
            warn!(
                "Dropping notify message exceeding {} bytes",
                Self::MAX_MESSAGE_SIZE
            );
            return false;
        }
        trace!(
            "Relaying notify message: {:?}",
            String::from_utf8_lossy(message)
        );
        true
    }
}

impl Drop for NotifySocket {
    fn drop(&mut self) {
        // The bundle usually gets removed anyway, this just does not leave a
        // dead socket behind in case it stays, and only the own one.
        match fs::metadata(&self.path) {
            Ok(metadata) if metadata.dev() == self.dev && metadata.ino() == self.ino => {
                if let Err(e) = fs::remove_file(&self.path) {
                    debug!("Unable to remove notify socket: {e}");
                }
            }
            Ok(_) => debug!("Notify socket got replaced, keeping it"),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => debug!("Unable to stat notify socket: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::{
            linux::net::SocketAddrExt,
            unix::{fs::FileTypeExt, net::SocketAddr},
        },
        time::Duration,
    };
    use tempfile::tempdir;
    use tokio::time;

    fn socket_path(bundle: &Path) -> PathBuf {
        bundle.join(NotifySocket::DIR).join(NotifySocket::FILE)
    }

    async fn recv_string(host: &UnixDatagram) -> Result<String> {
        let mut buf = [0; NotifySocket::MAX_MESSAGE_SIZE + 1];
        let len = time::timeout(Duration::from_secs(5), host.recv(&mut buf)).await??;
        Ok(String::from_utf8_lossy(&buf[..len]).into_owned())
    }

    #[tokio::test]
    async fn relays_messages_until_cancelled() -> Result<()> {
        let bundle = tempdir()?;
        let host_path = bundle.path().join("host.sock");
        let host = UnixDatagram::bind(&host_path)?;

        let sut = NotifySocket::bind(bundle.path(), host_path.to_str().context("path")?)?;
        let path = socket_path(bundle.path());
        assert_eq!(sut.path, path);
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o777);

        let token = CancellationToken::new();
        sut.spawn(token.clone());

        let container = UnixDatagram::unbound()?;
        container.send_to(b"READY=1\nSTATUS=up", &path).await?;
        assert_eq!(recv_string(&host).await?, "READY=1\nSTATUS=up");

        // Too long messages are dropped, the rest still arrives.
        let long = vec![b'x'; NotifySocket::MAX_MESSAGE_SIZE + 1];
        container.send_to(&long, &path).await?;
        container.send_to(b"STATUS=after", &path).await?;
        assert_eq!(recv_string(&host).await?, "STATUS=after");

        // A recreated host socket still receives.
        drop(host);
        fs::remove_file(&host_path)?;
        let host = UnixDatagram::bind(&host_path)?;
        container.send_to(b"STATUS=recreated", &path).await?;
        assert_eq!(recv_string(&host).await?, "STATUS=recreated");

        // Messages sent right before the exit are not lost.
        container.send_to(b"STOPPING=1", &path).await?;
        token.cancel();
        assert_eq!(recv_string(&host).await?, "STOPPING=1");

        time::timeout(Duration::from_secs(5), async {
            while path.exists() {
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn relays_to_an_abstract_host_socket() -> Result<()> {
        let bundle = tempdir()?;
        let name = format!("conmonrs-test-{}", std::process::id());
        let host = StdUnixDatagram::bind_addr(&SocketAddr::from_abstract_name(&name)?)?;
        host.set_nonblocking(true)?;
        let host = UnixDatagram::from_std(host)?;

        let sut = NotifySocket::bind(bundle.path(), &format!("@{name}"))?;
        let path = socket_path(bundle.path());
        let token = CancellationToken::new();
        sut.spawn(token.clone());

        let container = UnixDatagram::unbound()?;
        container.send_to(b"READY=1", &path).await?;
        assert_eq!(recv_string(&host).await?, "READY=1");

        token.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn bind_fails_without_host_socket() -> Result<()> {
        let bundle = tempdir()?;
        let host_path = bundle.path().join("missing.sock");
        let host = host_path.to_str().context("path")?;
        assert!(NotifySocket::bind(bundle.path(), host).is_err());
        assert!(!socket_path(bundle.path()).exists());
        Ok(())
    }

    #[tokio::test]
    async fn bind_replaces_a_stale_socket_but_not_a_live_one() -> Result<()> {
        let bundle = tempdir()?;
        let host_path = bundle.path().join("host.sock");
        let _host = UnixDatagram::bind(&host_path)?;
        let host = host_path.to_str().context("path")?;
        let path = socket_path(bundle.path());
        fs::create_dir(bundle.path().join(NotifySocket::DIR))?;
        fs::write(&path, b"stale")?;

        let first = NotifySocket::bind(bundle.path(), host)?;
        assert!(fs::metadata(&path)?.file_type().is_socket());

        // Another container of the bundle still uses the socket.
        assert!(NotifySocket::bind(bundle.path(), host).is_err());
        assert!(path.exists());

        drop(first);
        assert!(!path.exists());

        // The socket of a server which exited without cleaning up. A process
        // which a parallel test forks may hold it until it execs.
        let stale = StdUnixDatagram::bind(&path)?;
        drop(stale);
        let mut again = NotifySocket::bind(bundle.path(), host);
        for _ in 0..500 {
            match &again {
                Err(e) if format!("{e:#}").contains("in use") => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    again = NotifySocket::bind(bundle.path(), host);
                }
                _ => break,
            }
        }
        let again = again?;
        drop(again);
        assert!(!path.exists());
        Ok(())
    }
}
