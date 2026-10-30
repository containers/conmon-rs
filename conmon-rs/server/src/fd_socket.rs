//! File descriptor passing over a UNIX socket.
//!
//! ## File descriptor socket protocol
//!
//! Connect via a unix `SOCK_SEQPACKET` socket.
//!
//! **Request:**
//! Pass file descriptors via `SCM_RIGHTS` ancillary message.
//!
//! `request_id: u32`: Request id selected by client
//! `num_fds: u32`: Number of file descriptors, at most 253, the most the
//! kernel passes with one message (`SCM_MAX_FD`)
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
//!
//! A connection holds at most 1024 file descriptors which no request took
//! yet, and all connections together a quarter of the open files limit of
//! the server, a request which would exceed either fails with an error
//! response.

use crate::{attach::input_ended, listener, mutex::lock};
use anyhow::{Result, bail};
use nix::sys::resource::{Resource, getrlimit};
use std::{
    collections::{HashMap, HashSet, hash_map},
    io::{IoSliceMut, Write},
    mem,
    num::Wrapping,
    os::fd::{OwnedFd, RawFd},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{sync::Mutex as AsyncMutex, task};
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener, ancillary::OwnedAncillaryMessage};
use tracing::{Instrument, debug, debug_span};

/// The most descriptors the kernel passes with one message (`SCM_MAX_FD`),
/// which is also the limit of the Go client.
const MAX_FDS: usize = 253;

/// The most descriptors a connection holds at a time, which it sent and no
/// request took yet. A client which never lets go of them must not use up the
/// descriptors of the server, which every container needs.
const MAX_HELD_FDS: usize = 1024;

/// The share of the open files limit of the server which all connections
/// together may hold, many connections must not use them up either.
const HELD_SHARE_OF_LIMIT: u64 = 4;

/// How many descriptors connections may hold, see [`MAX_HELD_FDS`] and
/// [`HELD_SHARE_OF_LIMIT`].
#[derive(Clone, Copy, Debug)]
struct Limits {
    per_connection: usize,
    total: usize,
}

impl Limits {
    /// The limits of the server with its current open files limit.
    fn of_server() -> Self {
        let total = match getrlimit(Resource::RLIMIT_NOFILE) {
            Ok((soft, _)) => usize::try_from(soft / HELD_SHARE_OF_LIMIT).unwrap_or(usize::MAX),
            Err(e) => {
                debug!("Unable to get the open files limit: {e}");
                MAX_HELD_FDS
            }
        };
        Self {
            per_connection: MAX_HELD_FDS,
            total,
        }
    }
}

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
        let mut state = lock(&self.state);
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
                hash_map::Entry::Occupied(_) => {}
                hash_map::Entry::Vacant(entry) => {
                    entry.insert(fd);
                    break;
                }
            }
        }
        debug!("Added descriptor in slot {slot}");
        self.last = slot;
        slot.0
    }

    fn take(&mut self, slot: u64) -> Result<OwnedFd> {
        debug!("Taking descriptor of slot {slot}");
        self.fds
            .remove(&slot)
            .ok_or_else(|| anyhow::anyhow!("no file descriptor in slot {slot}"))
    }
}

#[derive(Debug)]
struct Server {
    path: PathBuf,
}

struct ConnectionGuard {
    fd_socket: Arc<FdSocket>,
    slots: Vec<u64>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.close();
    }
}

impl ConnectionGuard {
    fn close(&mut self) {
        let mut state = lock(&self.fd_socket.state);
        let slots = mem::take(&mut self.slots);
        debug!("Releasing the descriptors of {} slots", slots.len());
        for slot in slots {
            state.fds.remove(&slot);
        }
    }
}

impl Server {
    fn start(path: PathBuf, fd_socket: Arc<FdSocket>) -> Result<Self> {
        let server = Self { path };

        let mut listener = listener::bind_long_path(&server.path, UnixSeqpacketListener::bind)?;
        // After the server raised its open files limit at startup.
        let limits = Limits::of_server();

        task::spawn(
            async move {
                // One failed connection must not stop the socket, the loop
                // never ends.
                let mut accept_errors = listener::AcceptErrors::default();
                loop {
                    match listener.accept().await {
                        Ok(conn) => {
                            task::spawn(
                                Self::serve(conn, fd_socket.clone(), limits)
                                    .instrument(debug_span!("fd_socket_serve")),
                            );
                        }
                        Err(e) => accept_errors.failed(&e).await,
                    }
                }
            }
            .instrument(debug_span!("fd_socket_server")),
        );

        Ok(server)
    }

    /// Add the received descriptors to the state and the slots of the
    /// connection, all of them or none, and return their slots, within the
    /// `limits`.
    fn add_received_fds(
        guard: &mut ConnectionGuard,
        received_fds: Vec<OwnedFd>,
        num_fds: usize,
        limits: Limits,
    ) -> Result<Vec<u64>> {
        if received_fds.len() != num_fds {
            bail!(
                "received {} fds, but expected {num_fds} fds",
                received_fds.len()
            )
        }

        let mut state = lock(&guard.fd_socket.state);
        // Slots which requests took meanwhile do not count.
        guard.slots.retain(|slot| state.fds.contains_key(slot));
        if guard.slots.len() + received_fds.len() > limits.per_connection {
            bail!(
                "connection holds {} file descriptors already, the limit is {}",
                guard.slots.len(),
                limits.per_connection
            )
        }
        if state.fds.len() + received_fds.len() > limits.total {
            bail!(
                "connections hold {} file descriptors already, the limit is {}",
                state.fds.len(),
                limits.total
            )
        }
        let slots: Vec<u64> = received_fds.into_iter().map(|fd| state.add(fd)).collect();
        guard.slots.extend(&slots);
        Ok(slots)
    }

    async fn serve(conn: UnixSeqpacket, fd_socket: Arc<FdSocket>, limits: Limits) -> Result<()> {
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
                // An empty packet reads like the end of the connection, which
                // releases its descriptors, see `attach::input_ended`.
                0 if input_ended(&conn)? => break Ok(()),
                8 => u64::from_le_bytes(buf[..8].try_into().expect("8-byte slice")),
                _ => continue, // ignore invalid message
            };

            if id_and_num_fds == 0 {
                guard.close();
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

            let result = Self::add_received_fds(&mut guard, received_fds, num_fds, limits);

            match result {
                Ok(slots) => {
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
mod tests;
