//! The `conmonrs pause` subprocess, which unshares the namespaces of a pod
//! and holds them.

use super::{Namespace, Pause};
use crate::{config::PauseArgs, process::wait_child};
use anyhow::{Context, Result, bail};
use libc::{c_ulong, pid_t};
use nix::{
    errno::Errno,
    mount::{MntFlags, MsFlags, mount, umount2},
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sched::{CloneFlags, setns, unshare},
    sys::{
        prctl,
        signal::{SigSet, Signal, kill},
        wait::{Id, WaitPidFlag, waitpid},
    },
    unistd::{
        ForkResult, Gid, Pid, SysconfVar, Uid, chdir, dup2_stderr, fork, getpid, pivot_root,
        setresgid, setresuid, sysconf,
    },
};
use std::{
    fmt::Write as _,
    fs::{self, File},
    io::{Read, Write},
    net::Shutdown,
    os::{fd::AsFd, unix::net::UnixStream},
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
    /// that stays around, in an empty root without capabilities, see
    /// [`Pause::enter_empty_root`], [`Pause::drop_privileges`] and
    /// [`Pause::wait_for_binds`].
    #[expect(unsafe_code, reason = "forking")]
    pub(crate) fn run(args: &PauseArgs) -> Result<()> {
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

        // SAFETY: the pause process is single threaded at this point, it
        // starts no threads before, so no lock which the child needs can be
        // held by another thread. The child only unshares namespaces, talks
        // over the socket pair and forks the init, which is fine after a
        // fork, and its errors allocate only with no other thread around.
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
        let (holder, init_sock) = if pid {
            // Read before the fork, which allocates, while the mounts are
            // still the ones of the host.
            let empty_root = Self::root_can_pivot();
            if !empty_root {
                eprintln!(
                    "The root cannot be pivoted, pause init keeps the root and mounts of the host"
                );
            }
            // The init inherits the mask, so no signal arrives before it waits
            // for them: they stay pending until then.
            let signals = Self::init_signals();
            signals.thread_block().context("block init signals")?;
            // The init reports over it once it set up its root, and waits for
            // it to close, see `wait_for_binds`.
            let (mut init_sock, mut child_sock) =
                UnixStream::pair().context("create init socket pair")?;
            // SAFETY: the child of the pause process is single threaded like
            // the pause process, see above. The init only sets up its root,
            // drops its privileges, talks over the socket pair, waits for
            // signals and reaps after the fork, its errors allocate only with
            // no other thread around.
            match unsafe { fork().context("forking PID namespace init")? } {
                ForkResult::Parent { child } => {
                    signals.thread_unblock().context("unblock init signals")?;
                    drop(child_sock);
                    // Before the namespaces get bound, so that none of the
                    // bind mounts propagates into the mount namespace of the
                    // init. Nobody else stops the init if this fails: the
                    // parent does not know it yet, and there is no PID file.
                    if let Err(e) = Self::wait_for_init_root(&mut init_sock, &sock) {
                        if let Err(e) = kill(child, Signal::SIGKILL) {
                            debug!("Unable to kill pause init {child}: {e:#}");
                        }
                        return Err(e);
                    }
                    (child, Some(init_sock))
                }
                ForkResult::Child => {
                    drop(sock);
                    drop(init_sock);
                    // The server reads the errors of the setup from stderr
                    // until it ends, which the init must not hold open. If
                    // it does, the server waits for it a bounded time.
                    // /dev/null gets opened before the root is gone, stderr
                    // replaced after the namespaces got bound, so that a
                    // failure still reaches the server, which logs it. The
                    // init keeps running in the root and the mount namespace
                    // of the host then, where it can return to them, see
                    // `enter_empty_root`. It drops its privileges either way.
                    let null = File::open("/dev/null");
                    if empty_root && let Err(e) = Self::enter_empty_root() {
                        eprintln!("Unable to give pause init an empty root: {e:#}");
                    }
                    if let Err(e) = Self::drop_privileges() {
                        eprintln!("Unable to drop the privileges of pause init: {e:#}");
                    }
                    if let Err(e) = Self::wait_for_binds(&mut child_sock) {
                        eprintln!("Unable to make pause init not dumpable: {e:#}");
                    }
                    if let Ok(null) = null {
                        let _ = dup2_stderr(null);
                    }
                    // Lets the pause child exit.
                    drop(child_sock);
                    return Self::run_init(signals);
                }
            }
        } else {
            (getpid(), None)
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

        // The init makes itself not dumpable now and stops holding stderr,
        // before this process and the pause process exit, the server waits for
        // the end of stderr.
        if let Some(mut init_sock) = init_sock
            && let Err(e) = init_sock
                .shutdown(Shutdown::Write)
                .and_then(|()| init_sock.read_to_end(&mut vec![]))
        {
            eprintln!("Unable to wait for pause init {holder}: {e}");
        }

        // The bind mounts keep the namespaces alive on their own, only a PID
        // namespace dies with its init, which stays around.
        debug!("Namespaces bound, exiting pause child");
        Ok(())
    }

    /// Whether the root of this process can be pivoted, see
    /// [`Pause::pivot_to_empty_root`]. Assumed if the mounts cannot be read,
    /// a failed pivot returns to the mounts of the host where it can.
    fn root_can_pivot() -> bool {
        fs::read_to_string("/proc/self/mountinfo")
            .map_or(true, |mountinfo| Self::root_has_parent_mount(&mountinfo))
    }

    /// Whether the root in a `/proc/<pid>/mountinfo` is a mount point which
    /// is mounted on another mount, which pivot_root(2) needs. The root of
    /// an initramfs is the root mount of its mount namespace, which is its
    /// own parent, see proc_pid_mountinfo(5). A chroot into a directory which
    /// is not a mount point has no mount at `/`.
    fn root_has_parent_mount(mountinfo: &str) -> bool {
        // The ID and parent ID of the mounts at `/`.
        let roots: Vec<(&str, &str)> = mountinfo
            .lines()
            .filter_map(|line| {
                let mut fields = line.split(' ');
                let (id, parent) = (fields.next()?, fields.next()?);
                // Skips the device and the root of the mount.
                (fields.nth(2)? == "/").then_some((id, parent))
            })
            .collect();
        // The topmost one, which is no parent of another one.
        roots
            .iter()
            .find(|(id, _)| !roots.iter().any(|(_, parent)| parent == id))
            .is_some_and(|(id, parent)| id != parent)
    }

    /// Where the init of a PID namespace mounts its empty root, in a mount
    /// namespace of its own. A directory which every host has, the server
    /// runs the pause process through `/proc/self/exe`.
    const EMPTY_ROOT: &'static str = "/proc";

    /// Move into a mount namespace of its own with an empty, read-only root,
    /// for the init of a PID namespace. A process of the pod which may trace
    /// the init, like one with `CAP_SYS_PTRACE` without a user namespace,
    /// would reach the root of the host through `/proc/1/root` otherwise.
    /// The init needs no files, it only waits for signals and reaps. The
    /// parent of the pause process binds the other namespaces of the init
    /// from the host, which this does not change.
    ///
    /// On a failure, the init returns to the mount namespace of the host: a
    /// copy of it would keep all mounts of the host busy for the lifetime of
    /// the pod, and shared with them if making them private failed. With a
    /// user namespace, the init may not return, the mount namespace of the
    /// host belongs to the user namespace of the host. It stays in the copy
    /// then, which the error tells.
    fn enter_empty_root() -> Result<()> {
        // Closed on return.
        let host = File::open("/proc/self/ns/mnt").context("open mount namespace")?;
        unshare(CloneFlags::CLONE_NEWNS).context("unshare mount namespace")?;
        let Err(e) = Self::pivot_to_empty_root() else {
            return Ok(());
        };
        match setns(&host, CloneFlags::CLONE_NEWNS) {
            Ok(()) => Err(e),
            Err(errno) => Err(anyhow::format_err!(
                "{e:#}, staying in a copy of the host mount namespace, unable to return: {errno}"
            )),
        }
    }

    /// Replace the root of a mount namespace which [`Pause::enter_empty_root`]
    /// unshared with an empty tmpfs.
    fn pivot_to_empty_root() -> Result<()> {
        // Nothing of the following may reach the host.
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            None::<&str>,
        )
        .context("make mounts private")?;
        mount(
            Some("tmpfs"),
            Self::EMPTY_ROOT,
            Some("tmpfs"),
            MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            Some("size=4k,mode=0555"),
        )
        .context("mount empty root")?;
        chdir(Self::EMPTY_ROOT).context("change into empty root")?;
        // Stacks the old root on top of the new one, from where it gets
        // detached along with the mounts of the host, see pivot_root(2). A
        // chroot would not hide the host, `/proc/1/root/..` leads from the
        // tmpfs to where it is mounted. A root which cannot be pivoted, like
        // an initramfs, does not get here, see `root_can_pivot`.
        pivot_root(".", ".").context("pivot root")?;
        umount2(".", MntFlags::MNT_DETACH).context("detach old root")?;
        chdir("/").context("change into new root")
    }

    /// Drop all capabilities of the init of a PID namespace, also when its
    /// root could not be emptied. It only waits for signals and reaps, a
    /// process of the pod which may trace it could make it run syscalls
    /// with them otherwise, like mounting the disk of the host. It keeps its
    /// user ID, the server still signals it. Every step runs, the first
    /// failure gets returned. The init is single threaded, so the sets which
    /// capset(2) clears for its thread are the ones of the process. Clearing
    /// the permitted set clears the ambient one as well.
    #[expect(unsafe_code, reason = "prctl and capset without a safe wrapper")]
    fn drop_privileges() -> Result<()> {
        let no_new_privs = prctl::set_no_new_privs().context("set no new privileges");

        // Needs CAP_SETPCAP, before the effective set gets cleared. The
        // kernel rejects the first capability it does not know. Best effort:
        // a server without CAP_SETPCAP, like one with a trimmed bounding
        // set, cannot drop them, and the cleared sets below already keep
        // the init from using any, with no_new_privs it cannot regain them
        // through an execve either.
        let zero: c_ulong = 0;
        let mut bounding = Ok(());
        for cap in 0..c_ulong::from(u64::BITS) {
            // SAFETY: a prctl which takes no pointers.
            if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, zero, zero, zero) } == -1 {
                let errno = Errno::last();
                if !matches!(errno, Errno::EINVAL | Errno::EPERM) {
                    bounding = Err(errno).with_context(|| format!("drop capability {cap}"));
                }
                break;
            }
        }

        // The header of capset(2), `_LINUX_CAPABILITY_VERSION_3` for this
        // thread, and two entries of the effective, permitted and
        // inheritable sets, for 64 capabilities.
        let header: [u32; 2] = [0x2008_0522, 0];
        let data = [0u32; 6];
        // SAFETY: the kernel reads the header and the two entries which the
        // version takes, both outlive the call.
        let sets = Errno::result(unsafe {
            libc::syscall(libc::SYS_capset, header.as_ptr(), data.as_ptr())
        })
        .context("clear capability sets");

        [no_new_privs, bounding, sets.map(drop)]
            .into_iter()
            .collect()
    }

    /// Report to the pause child that the init set up its root and wait for
    /// the child to close its end of `sock`, which it does once the parent
    /// of the pause process bound the namespaces, or on a failure. Then make
    /// the init not dumpable: without capabilities, every process of the pod
    /// with its user ID could trace it otherwise. Only a process with
    /// `CAP_SYS_PTRACE` in the user namespace which the pause process got
    /// executed in may trace it or open its `/proc/<pid>/root` and
    /// `/proc/<pid>/ns` entries then. The parent of the pause process opens
    /// the latter before, so binding needs no `CAP_SYS_PTRACE`, and no
    /// process of the pod exists yet. A report or a wait which fails means
    /// that the child is gone, which failed the setup and kills the init, so
    /// only the dumpable flag can fail it. Allocates only for an error.
    fn wait_for_binds(sock: &mut UnixStream) -> Result<()> {
        let _ = sock.write_all(Self::MSG);
        let mut buf = [0];
        loop {
            match sock.read(&mut buf) {
                // Nothing gets sent, but it only waits for the end.
                Ok(1..) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(0) | Err(_) => break,
            }
        }
        prctl::set_dumpable(false).context("make it not dumpable")
    }

    /// Wait until the init reported over `init_sock` that it set up its
    /// root. Fails if the init died before, or once the parent of the pause
    /// process closed `parent`, like when the server killed it after the
    /// setup timeout: an init whose mounts stall would keep this process
    /// waiting forever otherwise. The parent sends nothing meanwhile.
    fn wait_for_init_root(init_sock: &mut UnixStream, parent: &UnixStream) -> Result<()> {
        let ready = |fd: &PollFd| fd.revents().is_some_and(|events| !events.is_empty());
        loop {
            let (root, parent_gone) = {
                let mut fds = [
                    PollFd::new(init_sock.as_fd(), PollFlags::POLLIN),
                    PollFd::new(parent.as_fd(), PollFlags::POLLIN),
                ];
                match poll(&mut fds, PollTimeout::NONE) {
                    Ok(_) => (ready(&fds[0]), ready(&fds[1])),
                    Err(Errno::EINTR) => continue,
                    Err(e) => return Err(e).context("wait for the root of pause init"),
                }
            };
            if root {
                return init_sock
                    .read_exact(&mut [0])
                    .context("wait for the root of pause init");
            }
            if parent_gone {
                bail!("pause process is gone while waiting for the root of pause init")
            }
        }
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
            // Also for the real-time signals, which nix does not know.
            match wait_child(Id::All, WaitPidFlag::WNOHANG) {
                Ok(None) | Err(Errno::ECHILD) => return,
                Ok(Some((pid, exit))) => {
                    trace!("Reaped child {pid} with exit code {}", exit.code());
                }
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
            writeln!(buf, "{container_id} {host_id} {size}")?;
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
        // it does not linger as a zombie, and so that its exit made the init a
        // child of the server, which checks that for the PID it reads. The
        // namespaces are already in place, so a failure is no reason to fail
        // the whole setup.
        if let Err(e) = waitpid(child, None) {
            eprintln!("Unable to wait for pause child {child}: {e:#}");
        }

        Ok(())
    }

    /// Undo a failed setup: kill the pause processes and remove the bind
    /// mounts and PID file which may already exist. Its failures do not get
    /// printed, the server adds stderr to the error of the setup, which
    /// must carry only its cause, and the subcommand runs without logging.
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
