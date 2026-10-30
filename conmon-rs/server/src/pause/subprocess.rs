//! The `conmonrs pause` subprocess, which unshares the namespaces of a pod
//! and holds them.

use super::{Namespace, Pause};
use crate::config::PauseArgs;
use anyhow::{Context, Result, bail};
use libc::pid_t;
use nix::{
    errno::Errno,
    sched::{CloneFlags, unshare},
    sys::{
        prctl,
        signal::{SigSet, Signal, kill},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::{
        ForkResult, Gid, Pid, SysconfVar, Uid, dup2_stderr, fork, getpid, setresgid, setresuid,
        sysconf,
    },
};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};
use tracing::{debug, trace, warn};

impl Pause {
    /// Run a new pause instance.
    ///
    /// The process forks. The child unshares the requested namespaces and the
    /// parent, which stays in the host namespaces, binds them from the
    /// `/proc/<pid>/ns` entries of the process holding them. Binding from the
    /// child would not work for a user namespace: its mounts live in a mount
    /// namespace owned by the less privileged user namespace and never reach
    /// the host.
    ///
    /// The bind mounts keep every namespace alive except a PID namespace, which
    /// dies with its init process. Unsharing a PID namespace does not move the
    /// caller into it, so the child forks that init, which is the only process
    /// that stays around.
    #[expect(unsafe_code, reason = "forking")]
    pub fn run(args: &PauseArgs) -> Result<()> {
        let &PauseArgs {
            ref base_path,
            ref pod_id,
            ipc,
            pid,
            net,
            user,
            uts,
            ref uid_mappings,
            ref gid_mappings,
        } = args;
        Self::validate_location(base_path, pod_id)?;
        let requested = [
            (ipc, Namespace::Ipc),
            (pid, Namespace::Pid),
            (net, Namespace::Net),
            (user, Namespace::User),
            (uts, Namespace::Uts),
        ];
        let namespaces: Vec<Namespace> = requested
            .into_iter()
            .filter_map(|(enabled, namespace)| enabled.then_some(namespace))
            .collect();

        let mut flags = CloneFlags::empty();
        if ipc {
            flags.insert(CloneFlags::CLONE_NEWIPC);
        }
        if pid {
            flags.insert(CloneFlags::CLONE_NEWPID);
        }
        if net {
            flags.insert(CloneFlags::CLONE_NEWNET);
        }
        if uts {
            flags.insert(CloneFlags::CLONE_NEWUTS);
        }

        // Become the subreaper of the init, so that a failed setup can reap
        // it after killing it. On success it gets reparented to the server once
        // this process exits.
        prctl::set_child_subreaper(true).context("set child subreaper")?;

        let (sock_parent, sock_child) = UnixStream::pair().context("create unix socket pair")?;

        // SAFETY: the pause process is single threaded at this point and the
        // child only unshares namespaces, talks over the socket pair and waits
        // for a signal, all of which is fine after a fork.
        match unsafe { fork().context("forking process")? } {
            ForkResult::Parent { child } => {
                // Close the child end, so that reads fail if the child dies.
                drop(sock_child);

                let mut parent = PauseParent {
                    base_path,
                    pod_id,
                    child,
                    namespaces: &namespaces,
                    holder: None,
                };
                let result = parent.run(uid_mappings, gid_mappings, sock_parent);
                if result.is_err() {
                    parent.cleanup();
                }
                result
            }
            ForkResult::Child => {
                drop(sock_parent);
                Self::run_child(flags, user, pid, sock_child)
            }
        }
    }

    /// The message exchanged between the pause parent and child.
    const MSG: &'static [u8] = &[1];

    /// The child side of the pause process: unshares the namespaces, forks the
    /// init of a PID namespace if requested, reports the process holding the
    /// namespaces and waits for the parent to bind them.
    #[expect(unsafe_code, reason = "forking the init of a PID namespace")]
    fn run_child(flags: CloneFlags, user: bool, pid: bool, mut sock: UnixStream) -> Result<()> {
        let mut res = [0];

        if user {
            unshare(CloneFlags::CLONE_NEWUSER).context("unshare into user namespace")?;

            // Notify that the user namespace is now created
            sock.write_all(Self::MSG)
                .context("notify about user namespace")?;

            // Wait for the mappings to be written
            sock.read_exact(&mut res).context("wait for mappings")?;

            // Set the UID and GID
            let uid = Uid::from_raw(0);
            setresuid(uid, uid, uid).context("set root uid")?;
            let gid = Gid::from_raw(0);
            setresgid(gid, gid, gid).context("set root gid")?;
        }

        // Unshare the rest of the namespaces after the user namespace, so that
        // it owns them.
        unshare(flags).context("unshare with clone flags")?;

        // Only the next child becomes part of a new PID namespace, as its init.
        let holder = if pid {
            // The init inherits the mask, so no signal arrives before it waits
            // for them: they stay pending until then.
            let signals = Self::init_signals();
            signals.thread_block().context("block init signals")?;
            // SAFETY: the child of the pause process is single threaded, the
            // init only waits for signals and reaps after the fork.
            match unsafe { fork().context("forking PID namespace init")? } {
                ForkResult::Parent { child } => {
                    signals.thread_unblock().context("unblock init signals")?;
                    child
                }
                ForkResult::Child => {
                    drop(sock);
                    // The server reads the errors of the setup from stderr
                    // until it ends, which the init must not hold open. If
                    // it does, the server waits for it a bounded time.
                    if let Ok(null) = File::open("/dev/null") {
                        let _ = dup2_stderr(null);
                    }
                    return Self::run_init(signals);
                }
            }
        } else {
            getpid()
        };

        // Report the process holding all namespaces and wait for the parent
        // to bind them.
        let result = sock
            .write_all(&holder.as_raw().to_ne_bytes())
            .context("report namespace holder PID")
            .and_then(|()| sock.read_exact(&mut res).context("wait for mounts"));

        // The parent may be gone, for example killed by the server shutdown,
        // then nobody else stops the init of the PID namespace.
        if result.is_err()
            && pid
            && let Err(e) = kill(holder, Signal::SIGKILL)
        {
            debug!("Unable to kill pause init {holder}: {e:#}");
        }
        result?;

        // The bind mounts keep the namespaces alive on their own, only a PID
        // namespace dies with its init, which stays around.
        debug!("Namespaces bound, exiting pause child");
        Ok(())
    }

    /// The init of the PID namespace, which keeps it alive until terminated.
    /// Processes of the pod which lose their parent get reparented to it, so it
    /// reaps them like any init.
    ///
    /// The signals are blocked since the fork and taken with sigwait, so none
    /// of them gets lost. Several exits may be reported by a single SIGCHLD,
    /// every one reaps all exited children.
    fn run_init(signals: SigSet) -> Result<()> {
        loop {
            match signals.wait() {
                Ok(Signal::SIGCHLD) => Self::reap_children(),
                Ok(signal) => {
                    debug!("Got signal {signal}, exiting pause init");
                    return Ok(());
                }
                Err(Errno::EINTR) => {}
                Err(e) => return Err(e).context("wait for signals"),
            }
        }
    }

    /// The signals the init waits for: the ones which terminate it and the
    /// exit of a child.
    fn init_signals() -> SigSet {
        let mut signals = SigSet::empty();
        for signal in [
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGCHLD,
        ] {
            signals.add(signal);
        }
        signals
    }

    /// Reap all exited children without blocking.
    fn reap_children() {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => return,
                Ok(status) => trace!("Reaped child: {status:?}"),
                Err(Errno::EINTR) => {}
                Err(e) => {
                    warn!("Unable to reap children: {e:#}");
                    return;
                }
            }
        }
    }

    /// The most ranges a mapping file takes (`UID_GID_MAP_MAX_EXTENTS`),
    /// Linux before 4.15 takes only 5.
    const MAX_MAPPINGS: usize = 340;

    /// Write user or group ID mappings. The kernel takes all ranges with a
    /// single write only, at offset 0 and shorter than a page. Nothing gets
    /// written without any, the kernel rejects an empty write.
    fn write_mappings(mappings: &[String], pid: Pid, is_group: bool) -> Result<()> {
        if mappings.is_empty() {
            return Ok(());
        }
        let page_size = sysconf(SysconfVar::PAGE_SIZE)
            .ok()
            .flatten()
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(4096);
        let buf = Self::mappings_buffer(mappings, page_size)?;

        let mut path = PathBuf::from("/proc");
        path.push(pid.as_raw().to_string());
        path.push(if is_group { "gid_map" } else { "uid_map" });

        let mut file = File::options()
            .write(true)
            .open(path)
            .context("open mapping file")?;

        let written = file.write(buf.as_bytes()).context("write mappings")?;
        if written != buf.len() {
            bail!("wrote {written} of {} bytes of the mappings", buf.len())
        }
        Ok(())
    }

    /// Validate the mappings and build the content of a mapping file from
    /// them, one `CONTAINER_ID HOST_ID SIZE` line per range.
    fn mappings_buffer(mappings: &[String], page_size: usize) -> Result<String> {
        if mappings.len() > Self::MAX_MAPPINGS {
            bail!(
                "{} mappings exceed the limit of {}",
                mappings.len(),
                Self::MAX_MAPPINGS
            )
        }

        let mut buf = String::new();
        for mapping in mappings {
            let fields: Vec<&str> = mapping.split_whitespace().collect();
            if fields.len() != 3 {
                bail!("mapping '{mapping}' has wrong format, expected 'CONTAINER_ID HOST_ID SIZE'");
            }
            let mut ids = [0u32; 3];
            for (id, field) in ids.iter_mut().zip(fields) {
                *id = field.parse().with_context(|| {
                    format!("mapping '{mapping}' has wrong format, expected all to be u32")
                })?;
            }
            let [container_id, host_id, size] = ids;
            buf.push_str(&format!("{container_id} {host_id} {size}\n"));
        }

        if buf.len() >= page_size {
            bail!(
                "mappings take {} bytes, the kernel takes less than {page_size}",
                buf.len()
            )
        }
        Ok(buf)
    }
}

/// The parent side of the pause process.
struct PauseParent<'a> {
    base_path: &'a Path,
    pod_id: &'a str,

    /// The forked pause child, which unshares the namespaces.
    child: Pid,

    /// The requested namespaces.
    namespaces: &'a [Namespace],

    /// The process holding the namespaces, once reported by the child. It is
    /// the init of the PID namespace if one was requested.
    holder: Option<Pid>,
}

impl PauseParent<'_> {
    /// Write the ID mappings, bind the namespaces of the holder process
    /// reported by the child and record the PID of the holder if it is the init
    /// of a PID namespace.
    fn run(
        &mut self,
        uid_mappings: &[String],
        gid_mappings: &[String],
        mut sock: UnixStream,
    ) -> Result<()> {
        let child = self.child;

        if self.namespaces.contains(&Namespace::User) {
            // Wait for user namespace creation
            let mut res = [0];
            sock.read_exact(&mut res)
                .context("wait for user namespace")?;

            // Write mappings
            Pause::write_mappings(gid_mappings, child, true).context("write gid maps")?;
            Pause::write_mappings(uid_mappings, child, false).context("write uid maps")?;

            // Notify that user mappings have been written
            sock.write_all(Pause::MSG)
                .context("notify about written mappings")?;
        }

        // Wait for the child to have all namespaces in place and to report
        // the process holding them.
        let mut buf = [0; size_of::<pid_t>()];
        sock.read_exact(&mut buf)
            .context("wait for namespace holder PID")?;
        let pid = Pid::from_raw(pid_t::from_ne_bytes(buf));
        self.holder = Some(pid);

        for namespace in self.namespaces {
            namespace
                .bind(self.base_path, self.pod_id, pid)
                .with_context(|| {
                    format!(
                        "bind namespace to path: {}",
                        namespace.path(self.base_path, self.pod_id).display(),
                    )
                })?;
        }

        if self.namespaces.contains(&Namespace::Pid) {
            let pause_pid_path = Pause::pause_pid_path(self.base_path, self.pod_id);
            fs::create_dir_all(
                pause_pid_path
                    .parent()
                    .context("no parent for pause PID path")?,
            )
            .context("create pause PID parent path")?;
            let mut file = File::create(pause_pid_path).context("create pause PID file")?;
            write!(file, "{pid}").context("write PID to pause file")?;
        }

        // Notify that all mounts are created
        sock.write_all(Pause::MSG).context("notify about mounts")?;

        // The child exits right after the notification. Reap it here so that
        // it does not linger as a zombie. The namespaces are already in place,
        // so a failure is no reason to fail the whole setup.
        if let Err(e) = waitpid(child, None) {
            warn!("Unable to wait for pause child {child}: {e:#}");
        }

        Ok(())
    }

    /// Undo a failed setup: kill the pause processes and remove the bind
    /// mounts and PID file which may already exist.
    fn cleanup(&self) {
        let child = self.child;

        let init = self.holder.filter(|holder| *holder != child);
        if let Some(init) = init
            && let Err(e) = kill(init, Signal::SIGKILL)
        {
            debug!("Unable to kill pause init {init}: {e:#}");
        }

        // Do not leave the child blocked on the socket or as a zombie.
        if let Err(e) = kill(child, Signal::SIGKILL) {
            debug!("Unable to kill pause child {child}: {e:#}");
        }
        if let Err(e) = waitpid(child, None) {
            debug!("Unable to wait for pause child {child}: {e:#}");
        }

        // With the child gone, the killed init got reparented to this process,
        // the subreaper.
        if let Some(init) = init
            && let Err(e) = waitpid(init, None)
        {
            debug!("Unable to wait for pause init {init}: {e:#}");
        }

        Pause::unbind(self.base_path, self.pod_id, self.namespaces);
    }
}

#[cfg(test)]
mod tests;
