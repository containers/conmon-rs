//! File descriptor passing via UDS (unix socket).
//!
//! ## File descriptor socket protocol
//!
//! Connect via a unix `SOCK_SEQPACKET` socket.
//!
//! **Request:**
//! Pass file descriptors via `SCM_RIGHTS` ancillary message.
//!
//! `request_id: u32`: Request id selected by client
//! `num_fds: u32`: Number of file descriptors
//!
//! ```txt
//! u64: (request_id << 32) | num_fds
//! ```
//!
//! **Response:**
//! ```txt
//! u64: (request_id << 32) | num_fds
//! u64: slot for file descriptor 0
//! u64: slot for file descriptor 1
//! ...
//! u64: slot for file descriptor num_fds-1
//! ```
//!
//! On error `num_fds` will be set to 0xffff_ffff and the rest of the message
//! is an error message string.
//! ```txt
//! u64: (request_id << 32) | 0xffff_ffff
//! ...: error message
//! ```
//!
//! **Close file descriptors:**
//! Received file descriptors will be closed on disconnect
//! or with an empty request (`request_id` = 0 and `num_fds` = 0).
//!
//! An empty request does **not** receive a response.

use crate::{listener, mutex::lock};
use anyhow::{Result, bail};
use std::{
    collections::{HashMap, HashSet, hash_map},
    io::{IoSliceMut, Write},
    mem,
    num::Wrapping,
    os::fd::{OwnedFd, RawFd},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{runtime::Handle, sync::Mutex as AsyncMutex, task};
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener, ancillary::OwnedAncillaryMessage};
use tracing::{Instrument, debug, debug_span};

/// The most descriptors the kernel passes with one message (`SCM_MAX_FD`),
/// which is also the limit of the Go client.
const MAX_FDS: usize = 253;

/// Room for a control message carrying `MAX_FDS` descriptors, more would
/// get cut off (`MSG_CTRUNC`).
#[expect(
    unsafe_code,
    clippy::cast_possible_truncation,
    reason = "CMSG_SPACE of a small constant length"
)]
// SAFETY: CMSG_SPACE only computes a size, it does not access any memory.
const ANCILLARY_BUF_SIZE: usize =
    unsafe { libc::CMSG_SPACE((MAX_FDS * size_of::<RawFd>()) as libc::c_uint) } as usize;

#[derive(Debug, Default)]
pub struct FdSocket {
    server: AsyncMutex<Option<Server>>,
    state: Mutex<State>,
}

impl FdSocket {
    pub async fn start(self: Arc<Self>, path: PathBuf) -> Result<PathBuf> {
        let mut server = self.server.lock().await;
        if let Some(server) = server.as_ref() {
            Ok(server.path.clone())
        } else {
            *server = Server::start(path.clone(), self.clone())?.into();
            Ok(path)
        }
    }

    /// Take the descriptors of all slots, or none of them if one is missing,
    /// so that a failed request leaves them for a retry.
    pub fn take_all<I>(&self, slots: I) -> Result<Vec<OwnedFd>>
    where
        I: IntoIterator<Item = u64>,
    {
        let slots: Vec<u64> = slots.into_iter().collect();
        if slots.is_empty() {
            return Ok(Vec::new());
        }
        let mut state = lock(&self.state)?;
        let mut seen = HashSet::with_capacity(slots.len());
        for slot in &slots {
            if !seen.insert(slot) {
                bail!("file descriptor slot {slot} requested twice")
            }
            if !state.fds.contains_key(slot) {
                bail!("no file descriptor in slot {slot}")
            }
        }
        slots.into_iter().map(|slot| state.take(slot)).collect()
    }
}

#[derive(Debug, Default)]
struct State {
    last: Wrapping<u64>,
    fds: HashMap<u64, OwnedFd>,
}

impl State {
    fn add(&mut self, fd: OwnedFd) -> u64 {
        let mut slot = self.last;
        loop {
            slot += 1;
            match self.fds.entry(slot.0) {
                hash_map::Entry::Occupied(_) => continue,
                hash_map::Entry::Vacant(entry) => {
                    entry.insert(fd);
                    break;
                }
            }
        }
        debug!("add {slot}: {:?}", self.fds);
        self.last = slot;
        slot.0
    }

    fn take(&mut self, slot: u64) -> Result<OwnedFd> {
        debug!("take {slot}: {:?}", self.fds);
        self.fds
            .remove(&slot)
            .ok_or_else(|| anyhow::anyhow!("no file descriptor in slot {slot}"))
    }
}

#[derive(Debug)]
struct Server {
    path: PathBuf,
}

/// Lets the next start bind a new socket once the accept loop task is gone,
/// for example on a panic. The loop itself does not stop on errors.
struct ListenerGuard(Arc<FdSocket>);

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        match Handle::try_current() {
            Ok(handle) => {
                let fd_socket = self.0.clone();
                handle.spawn(async move {
                    *fd_socket.server.lock().await = None;
                });
            }
            _ => {
                *self.0.server.blocking_lock() = None;
            }
        }
    }
}

struct ConnectionGuard {
    fd_socket: Arc<FdSocket>,
    slots: Vec<u64>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl ConnectionGuard {
    fn close(&mut self) -> Result<()> {
        let mut state = lock(&self.fd_socket.state)?;
        debug!("close... {state:?}");
        for slot in mem::take(&mut self.slots) {
            state.fds.remove(&slot);
        }
        debug!("closed: {state:?}");
        Ok(())
    }
}

impl Server {
    fn start(path: PathBuf, fd_socket: Arc<FdSocket>) -> Result<Self> {
        let server = Self { path };

        let mut listener = listener::bind_long_path(&server.path, UnixSeqpacketListener::bind)?;
        let guard = ListenerGuard(fd_socket);

        task::spawn(
            async move {
                // One failed connection must not stop the socket.
                loop {
                    match listener.accept().await {
                        Ok(conn) => {
                            let fd_socket = guard.0.clone();
                            task::spawn(
                                Self::serve(conn, fd_socket)
                                    .instrument(debug_span!("fd_socket_serve")),
                            );
                        }
                        Err(e) => listener::accept_failed(&e).await,
                    }
                }
            }
            .instrument(debug_span!("fd_socket_server")),
        );

        Ok(server)
    }

    /// Add the received descriptors to the state and the slots of the
    /// connection, all of them or none.
    fn add_received_fds(
        guard: &mut ConnectionGuard,
        received_fds: Vec<OwnedFd>,
        num_fds: usize,
    ) -> Result<()> {
        if received_fds.len() != num_fds {
            anyhow::bail!(
                "received {} fds, but expected {num_fds} fds",
                received_fds.len()
            )
        }

        let mut state = lock(&guard.fd_socket.state)?;
        guard
            .slots
            .extend(received_fds.into_iter().map(|fd| state.add(fd)));
        Ok(())
    }

    async fn serve(conn: UnixSeqpacket, fd_socket: Arc<FdSocket>) -> Result<()> {
        let mut guard = ConnectionGuard {
            fd_socket,
            slots: Vec::new(),
        };
        loop {
            let mut buf = [0; 9];
            let mut ancillary_buf = [0; ANCILLARY_BUF_SIZE];
            let (n, ancillary) = conn
                .recv_vectored_with_ancillary(&mut [IoSliceMut::new(&mut buf)], &mut ancillary_buf)
                .await?;

            let id_and_num_fds = match n.bytes_read() {
                0 => break Ok(()), // EOF
                8 => u64::from_le_bytes(buf[..8].try_into().expect("8-byte slice")),
                _ => continue, // ignore invalid message
            };

            if id_and_num_fds == 0 {
                guard.close()?;
                continue;
            }

            let num_fds = (id_and_num_fds & 0xffff_ffff) as usize;

            // Sized by what arrived, not by the count the peer claims.
            let mut received_fds = Vec::new();
            for msg in ancillary.into_messages() {
                if let OwnedAncillaryMessage::FileDescriptors(msg) = msg {
                    received_fds.extend(msg);
                }
                // other messages are ignored
            }

            let start = guard.slots.len();
            let result = Self::add_received_fds(&mut guard, received_fds, num_fds);

            match result {
                Ok(()) => {
                    let slots = &guard.slots[start..];
                    let mut buf = vec![0; 8 + slots.len() * 8];
                    let (chunks, _) = buf.as_chunks_mut::<8>();
                    chunks[0].copy_from_slice(&id_and_num_fds.to_le_bytes());
                    for (i, slot) in slots.iter().enumerate() {
                        chunks[i + 1].copy_from_slice(&slot.to_le_bytes());
                    }
                    conn.send(&buf).await?;
                }
                Err(err) => {
                    let mut buf = (id_and_num_fds | 0xffff_ffff).to_le_bytes().to_vec();
                    write!(buf, "{err}")?;
                    conn.send(&buf).await?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, io::IoSlice, os::fd::AsFd};
    use tokio_seqpacket::ancillary::AncillaryMessageWriter;

    /// Send a request for `num_fds` with the given descriptors attached and
    /// return the response.
    async fn request(
        conn: &UnixSeqpacket,
        id: u64,
        num_fds: u64,
        fds: &[OwnedFd],
    ) -> Result<Vec<u8>> {
        send(conn, &((id << 32) | num_fds).to_le_bytes(), fds).await?;
        let mut buf = [0; 8 + MAX_FDS * 8 + 1];
        let n = conn.recv(&mut buf).await?.bytes_read();
        Ok(buf[..n].to_vec())
    }

    /// Send a message with the given descriptors attached.
    async fn send(conn: &UnixSeqpacket, message: &[u8], fds: &[OwnedFd]) -> Result<()> {
        let mut ancillary_buf = [0; ANCILLARY_BUF_SIZE];
        let mut ancillary = AncillaryMessageWriter::new(&mut ancillary_buf);
        ancillary.add_fds(fds.iter().map(AsFd::as_fd))?;
        conn.send_vectored_with_ancillary(&[IoSlice::new(message)], &mut ancillary)
            .await?;
        Ok(())
    }

    /// Start an fd socket and connect to it.
    async fn connect() -> Result<(tempfile::TempDir, Arc<FdSocket>, UnixSeqpacket)> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("fd.sock");
        let fd_socket = Arc::new(FdSocket::default());
        fd_socket.clone().start(path.clone()).await?;
        let conn = UnixSeqpacket::connect(&path).await?;
        Ok((dir, fd_socket, conn))
    }

    fn null_fds(n: usize) -> Result<Vec<OwnedFd>> {
        (0..n)
            .map(|_| Ok(File::open("/dev/null")?.into()))
            .collect()
    }

    /// Whether the slot still holds a descriptor, without taking it.
    fn has_slot(fd_socket: &FdSocket, slot: u64) -> bool {
        lock(&fd_socket.state).is_ok_and(|state| state.fds.contains_key(&slot))
    }

    #[tokio::test]
    async fn empty_request_releases_the_slots() -> Result<()> {
        let (_dir, fd_socket, conn) = connect().await?;
        let resp = request(&conn, 1, 2, &null_fds(2)?).await?;
        let slots = [word(&resp, 1), word(&resp, 2)];

        // An empty request gets no response, the next one tells that it got
        // handled.
        send(&conn, &0u64.to_le_bytes(), &[]).await?;
        let resp = request(&conn, 2, 1, &null_fds(1)?).await?;
        assert_eq!(word(&resp, 0), (2 << 32) | 1);
        assert!(slots.iter().all(|slot| !has_slot(&fd_socket, *slot)));
        assert!(has_slot(&fd_socket, word(&resp, 1)));
        Ok(())
    }

    #[tokio::test]
    async fn disconnect_releases_the_slots() -> Result<()> {
        let (_dir, fd_socket, conn) = connect().await?;
        let resp = request(&conn, 1, 1, &null_fds(1)?).await?;
        let slot = word(&resp, 1);
        assert!(has_slot(&fd_socket, slot));

        drop(conn);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while has_slot(&fd_socket, slot) {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn messages_of_other_sizes_are_ignored() -> Result<()> {
        let (_dir, fd_socket, conn) = connect().await?;
        // Too short and too long, with a descriptor which must not end up in
        // a slot.
        send(&conn, &[1; 4], &null_fds(1)?).await?;
        send(&conn, &[1; 16], &null_fds(1)?).await?;

        let resp = request(&conn, 3, 1, &null_fds(1)?).await?;
        assert_eq!(word(&resp, 0), (3 << 32) | 1);
        assert_eq!(lock(&fd_socket.state)?.fds.len(), 1);
        Ok(())
    }

    fn word(buf: &[u8], i: usize) -> u64 {
        u64::from_le_bytes(buf[i * 8..(i + 1) * 8].try_into().expect("8 bytes"))
    }

    #[tokio::test]
    async fn fds_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("fd.sock");
        let fd_socket = Arc::new(FdSocket::default());
        assert_eq!(fd_socket.clone().start(path.clone()).await?, path);
        let conn = UnixSeqpacket::connect(&path).await?;

        let fds: Vec<OwnedFd> = vec![
            File::open("/dev/null")?.into(),
            File::open("/dev/null")?.into(),
        ];
        let resp = request(&conn, 7, 2, &fds).await?;
        assert_eq!(resp.len(), 3 * 8);
        assert_eq!(word(&resp, 0), (7 << 32) | 2);
        let slots = [word(&resp, 1), word(&resp, 2)];

        // A request which does not match the descriptors fails as a whole.
        let resp = request(&conn, 8, 3, &fds[..1]).await?;
        assert_eq!(word(&resp, 0), (8 << 32) | 0xffff_ffff);
        assert!(String::from_utf8_lossy(&resp[8..]).contains("expected 3 fds"));

        // Taking is all or nothing.
        assert!(fd_socket.take_all([slots[0], 999]).is_err());
        assert!(fd_socket.take_all([slots[0], slots[0]]).is_err());
        assert_eq!(fd_socket.take_all(slots)?.len(), 2);
        assert!(fd_socket.take_all([slots[0]]).is_err());
        assert!(fd_socket.take_all([])?.is_empty());

        // The most descriptors a message can carry all arrive.
        let fds = (0..MAX_FDS)
            .map(|_| Ok(File::open("/dev/null")?.into()))
            .collect::<Result<Vec<OwnedFd>>>()?;
        let resp = request(&conn, 9, MAX_FDS as u64, &fds).await?;
        assert_eq!(word(&resp, 0), (9 << 32) | MAX_FDS as u64);
        assert_eq!(resp.len(), (MAX_FDS + 1) * 8);
        Ok(())
    }
}
