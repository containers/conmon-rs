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
//! `num_fds: u8`: Number of file descriptors
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

use crate::{
    inactivity::Inactivity,
    listener::{Listener, SeqpacketListener},
};
use anyhow::Result;
use std::{
    collections::{hash_map, HashMap},
    io::{IoSliceMut, Write},
    mem,
    num::Wrapping,
    os::fd::OwnedFd,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{runtime::Handle, sync::Mutex as AsyncMutex, task};
use tokio_seqpacket::{ancillary::OwnedAncillaryMessage, UnixSeqpacket};
use tracing::{debug, debug_span, error, Instrument};

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
            *server = Server::start(path.clone(), self.clone()).await?.into();
            Ok(path)
        }
    }

    #[allow(dead_code)]
    pub fn take(&self, slot: u64) -> Result<OwnedFd> {
        lock!(self.state).take(slot)
    }

    pub fn take_all<I>(&self, slots: I) -> Result<Vec<OwnedFd>>
    where
        I: IntoIterator<Item = u64>,
        I::IntoIter: ExactSizeIterator,
    {
        let slots = slots.into_iter();
        if slots.len() == 0 {
            Ok(Vec::new())
        } else {
            let mut state = lock!(self.state);
            slots.into_iter().map(|slot| state.take(slot)).collect()
        }
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
        println!("add {slot}: {:?}", self.fds);
        self.last = slot;
        slot.0
    }

    fn take(&mut self, slot: u64) -> Result<OwnedFd> {
        println!("take {slot}: {:?}", self.fds);
        self.fds
            .remove(&slot)
            .ok_or_else(|| anyhow::anyhow!("no file descriptor in slot {slot}"))
    }
}

#[derive(Debug)]
struct Server {
    path: PathBuf,
}

struct ListenerGuard(Arc<FdSocket>);

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        if let Ok(handle) = Handle::try_current() {
            let fd_socket = self.0.clone();
            handle.spawn(async move {
                *fd_socket.server.lock().await = None;
            });
        } else {
            *self.0.server.blocking_lock() = None;
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
        let mut state = lock!(self.fd_socket.state);
        println!("close... {state:?}");
        for slot in mem::take(&mut self.slots) {
            state.fds.remove(&slot);
        }
        println!("closed: {state:?}");
        Ok(())
    }
}

impl Server {
    async fn start(path: PathBuf, fd_socket: Arc<FdSocket>) -> Result<Self> {
        let server = Self { path };

        let mut listener = Listener::<SeqpacketListener>::default().bind_long_path(&server.path)?;
        let guard = ListenerGuard(fd_socket);

        let inactivity = Inactivity::new();
        let timeout = Duration::from_secs(3);

        task::spawn(
            async move {
                loop {
                    let conn = tokio::select! {
                        () = inactivity.wait(timeout) => {
                            debug!("Stop fd socket after inactivity");
                            break;
                        }
                        conn = listener.accept() => match conn {
                            Ok(conn) => conn,
                            Err(err) => {
                                error!("Unable to accept on fd socket: {err}");
                                break;
                            }
                        },
                    };
                    let fd_socket = guard.0.clone();
                    let activity = inactivity.activity();
                    task::spawn(
                        async move {
                            let result = Self::serve(conn, fd_socket).await;
                            activity.stop();
                            result
                        }
                        .instrument(debug_span!("fd_socket_serve")),
                    );
                }
                drop(guard);
            }
            .instrument(debug_span!("fd_socket_server")),
        );

        Ok(server)
    }

    async fn serve(conn: UnixSeqpacket, fd_socket: Arc<FdSocket>) -> Result<()> {
        let mut guard = ConnectionGuard {
            fd_socket,
            slots: Vec::new(),
        };
        loop {
            let mut buf = [0; 9];
            let mut ancillary_buf = [0; 1024];
            let (n, ancillary) = conn
                .recv_vectored_with_ancillary(&mut [IoSliceMut::new(&mut buf)], &mut ancillary_buf)
                .await?;

            let id_and_num_fds = match n {
                0 => break Ok(()), // EOF
                8 => u64::from_le_bytes(buf[..8].try_into().unwrap()),
                _ => continue, // ignore invalid message
            };

            if id_and_num_fds == 0 {
                guard.close()?;
                continue;
            }

            let num_fds = (id_and_num_fds & 0xff) as usize;

            let result: Result<_> = async {
                let mut received_fds = Vec::with_capacity(num_fds);
                for msg in ancillary.into_messages() {
                    if let OwnedAncillaryMessage::FileDescriptors(msg) = msg {
                        received_fds.extend(msg);
                    } else {
                        // ignore other messages
                    }
                }

                if received_fds.len() != num_fds {
                    anyhow::bail!(
                        "received {} fds, but expected {num_fds} fds",
                        received_fds.len()
                    )
                }

                let mut state = lock!(guard.fd_socket.state);
                let start = guard.slots.len();
                guard
                    .slots
                    .extend(received_fds.into_iter().map(|fd| state.add(fd)));
                Ok(&guard.slots[start..])
            }
            .await;

            match result {
                Ok(slots) => {
                    let mut buf = vec![0; 8 + slots.len() * 8];
                    let mut chunks = buf.chunks_exact_mut(8);
                    chunks
                        .next()
                        .unwrap()
                        .copy_from_slice(&id_and_num_fds.to_le_bytes());
                    for slot in slots {
                        chunks.next().unwrap().copy_from_slice(&slot.to_le_bytes());
                    }
                    conn.send(&buf).await?;
                }
                Err(err) => {
                    let mut buf = (id_and_num_fds | 0xff).to_le_bytes().to_vec();
                    write!(buf, "{err}")?;
                    conn.send(&buf).await?;
                }
            }
        }
    }
}
