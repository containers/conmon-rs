//! The thread which creates the sockets of a session in the network
//! namespace of its pod.

use super::{CONNECT_BUDGET, SOCKET_THREAD_LINGER, SOCKET_THREAD_STACK_SIZE};
use crate::{mutex::lock, streaming_server::budget::Budget};
use anyhow::{Context, Result, bail};
use nix::{
    sched::{CloneFlags, setns},
    sys::socket::{AddressFamily, SockFlag, SockType, socket},
};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    fs::{self, File},
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream as StdTcpStream},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, mpsc as std_mpsc},
    thread,
};
use tokio::{sync::oneshot, time::Duration};

/// The sockets for a connection into the pod, one per address family.
type Sockets = Vec<(IpAddr, io::Result<StdTcpStream>)>;

/// The requests for sockets to the thread in the network namespace.
type SocketRequests = std_mpsc::Sender<oneshot::Sender<Result<Sockets>>>;

/// Creates the sockets of a session in its network namespace, which stay in
/// the namespace they got created in. The runtime connects them. A thread
/// enters the namespace and serves the connections which follow within the
/// linger time, then it ends and leaves the namespace, at once when the
/// session ends. Failures are not kept, every connection which fails gets its
/// own error.
pub(super) struct Namespace {
    pub(super) path: PathBuf,
    linger: Duration,

    /// The requests of the thread which got started last.
    requests: Mutex<Option<SocketRequests>>,

    /// What the connections which get established hold, see
    /// [`CONNECT_BUDGET`].
    pub(super) connect_budget: Budget,

    /// How many threads got started and ended.
    #[cfg(test)]
    pub(super) threads: AtomicUsize,
    #[cfg(test)]
    pub(super) ended: Arc<AtomicUsize>,
}

impl Namespace {
    pub(super) fn new(path: PathBuf) -> Arc<Self> {
        Self::with_linger(path, SOCKET_THREAD_LINGER)
    }

    pub(super) fn with_linger(path: PathBuf, linger: Duration) -> Arc<Self> {
        Arc::new(Self {
            path,
            linger,
            requests: Mutex::default(),
            connect_budget: Budget::new(CONNECT_BUDGET),
            #[cfg(test)]
            threads: AtomicUsize::new(0),
            #[cfg(test)]
            ended: Arc::default(),
        })
    }

    pub(super) async fn sockets(&self) -> Result<Sockets> {
        // A thread which ended before it took the request gets replaced
        // once.
        for _ in 0..2 {
            let (tx, rx) = oneshot::channel();
            self.send(tx)?;
            if let Ok(sockets) = rx.await {
                return sockets;
            }
        }
        bail!("socket thread ended")
    }

    /// Send a request to the thread, which gets started if it ended.
    fn send(&self, reply: oneshot::Sender<Result<Sockets>>) -> Result<()> {
        let running = self.lock().clone();
        let reply = match running {
            Some(requests) => match requests.send(reply) {
                Ok(()) => return Ok(()),
                Err(std_mpsc::SendError(reply)) => reply,
            },
            None => reply,
        };
        let requests = self.start()?;
        requests
            .send(reply)
            .map_err(|_| anyhow::anyhow!("socket thread ended"))?;
        *self.lock() = Some(requests);
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Option<SocketRequests>> {
        lock(&self.requests)
    }

    fn start(&self) -> Result<SocketRequests> {
        let (tx, rx) = std_mpsc::channel();
        let (path, linger) = (self.path.clone(), self.linger);
        #[cfg(test)]
        let ended = self.ended.clone();
        thread::Builder::new()
            .name("portforward".into())
            .stack_size(SOCKET_THREAD_STACK_SIZE)
            .spawn(move || {
                Self::serve(&path, linger, &rx);
                #[cfg(test)]
                ended.fetch_add(1, Ordering::Relaxed);
            })
            .context("spawn socket thread")?;
        #[cfg(test)]
        self.threads.fetch_add(1, Ordering::Relaxed);
        Ok(tx)
    }

    /// Create sockets until no request arrived for the linger time or the
    /// session is gone. The thread enters the namespace for its first
    /// request, a request which finds it outside of the namespace tries again
    /// and gets its own error.
    fn serve(
        path: &Path,
        linger: Duration,
        requests: &std_mpsc::Receiver<oneshot::Sender<Result<Sockets>>>,
    ) {
        let mut entered = false;
        while let Ok(reply) = requests.recv_timeout(linger) {
            if !entered {
                if let Err(e) = Self::enter(path) {
                    let _ = reply.send(Err(e));
                    continue;
                }
                entered = true;
            }
            let _ = reply.send(Ok(Self::create_sockets()));
        }
    }

    /// Enter the network namespace with the current thread.
    fn enter(path: &Path) -> Result<()> {
        let netns = File::open(path)
            .with_context(|| format!("open network namespace {}", path.display()))?;
        // Entering the own namespace needs privileges as well, which a server
        // for a host network pod or in tests may not have.
        let current = fs::metadata("/proc/thread-self/ns/net").context("stat network namespace")?;
        let target = netns.metadata().context("stat target network namespace")?;
        if (current.dev(), current.ino()) != (target.dev(), target.ino()) {
            setns(&netns, CloneFlags::CLONE_NEWNET)
                .with_context(|| format!("enter network namespace {}", path.display()))?;
        }
        Ok(())
    }

    /// A socket of each family, for one connection. The IPv6 one is only
    /// needed if IPv4 fails, creating it right away costs less than another
    /// round trip into the namespace thread.
    fn create_sockets() -> Sockets {
        [
            (AddressFamily::Inet, IpAddr::V4(Ipv4Addr::LOCALHOST)),
            (AddressFamily::Inet6, IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ]
        .into_iter()
        .map(|(family, ip)| {
            let socket = socket(
                family,
                SockType::Stream,
                SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
                None,
            )
            .map(StdTcpStream::from)
            .map_err(io::Error::from);
            (ip, socket)
        })
        .collect()
    }
}
