#![deny(missing_docs)]

use crate::conmon_capnp::conmon::{self, CgroupManager};
#[cfg(feature = "tracing")]
use crate::telemetry::Telemetry;
use crate::{
    child_reaper::ChildReaper,
    config::{Config, LogDriver, LogLevel},
    container_io::{ContainerIO, ContainerIOType},
    fd_socket::FdSocket,
    init::{self, DefaultInit, Init},
    journal::Journal,
    listener::{self, DefaultListener, Listener},
    pause::Pause,
    streaming_server::StreamingServer,
};
use anyhow::{Context, Result, format_err};
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp::Side, twoparty};
use futures::{AsyncReadExt, FutureExt};
use libc::_exit;
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill},
    unistd::{ForkResult, fork},
};
#[cfg(feature = "tracing")]
use opentelemetry::trace::{FutureExt as OpenTelemetryFutureExt, TracerProvider};
#[cfg(feature = "tracing")]
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::{fs::File, io::Write, os::fd::RawFd, path::Path, sync::Arc};
use tokio::{
    fs,
    runtime::{Builder, Handle},
    signal::unix::{SignalKind, signal},
    sync::{RwLock, oneshot},
    task::{self, LocalSet},
};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{Instrument, Span, debug, debug_span, info};
#[cfg(feature = "tracing")]
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, prelude::*};
use twoparty::VatNetwork;

/// The tracer provider created by `init_logging`, if the feature is enabled.
#[cfg(feature = "tracing")]
type Tracer = Option<SdkTracerProvider>;
#[cfg(not(feature = "tracing"))]
type Tracer = ();

#[derive(Debug)]
/// The main server structure.
pub struct Server {
    /// Server configuration.
    config: Arc<Config>,

    /// Child reaper instance.
    reaper: Arc<ChildReaper>,

    /// Fd socket instance.
    fd_socket: Arc<FdSocket>,

    /// OpenTelemetry tracer instance.
    #[cfg(feature = "tracing")]
    tracer: Option<SdkTracerProvider>,

    /// Streaming server instance.
    streaming_server: Arc<RwLock<StreamingServer>>,
}

impl Server {
    /// Server configuration.
    pub(crate) fn config(&self) -> &Arc<Config> {
        &self.config
    }

    /// Child reaper instance.
    pub(crate) fn reaper(&self) -> &Arc<ChildReaper> {
        &self.reaper
    }

    /// Fd socket instance.
    pub(crate) fn fd_socket(&self) -> &Arc<FdSocket> {
        &self.fd_socket
    }

    /// Streaming server instance.
    pub(crate) fn streaming_server(&self) -> &Arc<RwLock<StreamingServer>> {
        &self.streaming_server
    }

    /// Create a new server for a validated and prepared configuration.
    pub fn new(config: Config) -> Result<Self> {
        Self::init().context("init self")?;
        Ok(Self {
            config: Arc::new(config),
            reaper: Default::default(),
            fd_socket: Default::default(),
            #[cfg(feature = "tracing")]
            tracer: Default::default(),
            streaming_server: Default::default(),
        })
    }

    /// Close file descriptors inherited from whatever started us.
    ///
    /// conmon-rs daemonizes and outlives the process that spawned it, so any
    /// descriptor it inherits stays open for the lifetime of the server. If
    /// the spawning process was itself started with extra descriptors open,
    /// holding them can keep that process tree from ever finishing - a test
    /// harness waiting for its own pipe to close, for example.
    ///
    /// conmon does the same thing, see `close_all_fds_ge_than(3)` there. Any
    /// descriptor conmon-rs actually needs is either created after this point
    /// or received later over the fd socket, so nothing above stderr is worth
    /// keeping.
    fn close_inherited_fds() {
        for fd in Self::inherited_fds() {
            // The descriptor used to enumerate them is already closed by now,
            // so check before closing to avoid closing an unrelated descriptor
            // that reused the number. This runs before tokio starts, so nothing
            // else can be opening descriptors concurrently.
            //
            // These are raw libc calls rather than the `nix` wrappers on
            // purpose: `BorrowedFd`/`OwnedFd` require the descriptor to be open
            // for the duration of the borrow, and whether it is still open is
            // exactly what is unknown here. `fcntl(2)` and `close(2)` take a
            // plain integer with no such precondition and report `EBADF`.
            //
            // SAFETY: both calls are defined for any integer; a stale or
            // invalid descriptor number fails with `EBADF` rather than
            // affecting anything else.
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1 {
                unsafe { libc::close(fd) };
            }
        }
    }

    /// Every open descriptor above stderr. Split out from `close_inherited_fds`
    /// so it can be tested without closing the test runner's own descriptors.
    ///
    /// This is Linux only: it enumerates `/proc/self/fd` and would report
    /// nothing at all where that is unavailable, silently leaking the inherited
    /// descriptors. conmon-rs already requires Linux elsewhere (`prctl`, epoll,
    /// journald), so the assertion below just makes a port fail here loudly
    /// rather than lose the behaviour quietly. A log line would not work,
    /// `close_inherited_fds` runs well before `init_logging`.
    fn inherited_fds() -> Vec<RawFd> {
        const {
            assert!(
                cfg!(target_os = "linux"),
                "inherited fds are enumerated through /proc/self/fd, which is Linux only"
            )
        };

        let mut fds = Vec::new();

        if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
            for entry in entries.flatten() {
                if let Some(fd) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<RawFd>().ok())
                    && fd > 2
                {
                    fds.push(fd);
                }
            }
        }

        fds
    }

    /// Upper bound for the threads of the blocking pool. One of them runs
    /// the RPC `LocalSet` permanently, the rest serve file I/O like log
    /// writes, exit files and journald. Without a bound a burst of container
    /// exits or log reopens spawns a thread per operation (tokio's default
    /// allows 512), each with its own stack. Work which
    /// blocks for an unbounded time, like waiting for a process, must run on
    /// a dedicated thread instead, otherwise it can starve the pool.
    const MAX_BLOCKING_THREADS: usize = 16;

    /// Start the `Server` instance and consume it.
    pub fn start(self) -> Result<()> {
        // We need to fork as early as possible, especially before setting up tokio.
        // If we don't, the child will have a strange thread space and we're at risk of deadlocking.
        // We also have to treat the parent as the child (as described in [1]) to ensure we don't
        // interrupt the child's execution.
        // 1: https://docs.rs/nix/0.23.0/nix/unistd/fn.fork.html#safety
        if !self.config().skip_fork() {
            // SAFETY: nothing else runs yet, the tokio runtime is built only
            // after the fork, so the child does not inherit a half initialized
            // thread pool. The parent only writes the PID file and exits.
            match unsafe { fork()? } {
                ForkResult::Parent { child, .. } => {
                    let pidfile = self.config().conmon_pidfile();
                    if let Err(e) = File::create(&pidfile).and_then(|mut f| write!(f, "{child}")) {
                        // Without the PID file nobody knows the server, do not
                        // leave it running behind a failure.
                        eprintln!("Unable to write PID file {}: {e}", pidfile.display());
                        let _ = kill(child, Signal::SIGKILL);
                        unsafe { _exit(1) };
                    }
                    unsafe { _exit(0) };
                }
                ForkResult::Child => (),
            }
        }

        // Deliberately outside the `skip_fork` check above: the server outlives
        // its parent either way, so the descriptors have to go either way.
        Self::close_inherited_fds();

        // now that we've forked, set self to childreaper
        let ret = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        if ret != 0 {
            return Err(Errno::last()).context("set child subreaper");
        }

        debug!("Configuring Tokio runtime with current_thread");
        let rt = Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .max_blocking_threads(Self::MAX_BLOCKING_THREADS)
            .build()?;
        #[cfg(feature = "tracing")]
        let tracer = rt.block_on(self.spawn_tasks())?;
        #[cfg(not(feature = "tracing"))]
        rt.block_on(self.spawn_tasks())?;

        // Exporting the remaining spans needs the runtime the exporter was
        // created on, so shut down from a blocking task while it is driven.
        #[cfg(feature = "tracing")]
        if let Some(tracer) = tracer {
            rt.block_on(task::spawn_blocking(move || tracer.shutdown()))
                .context("join tracer shutdown")?
                .context("shutdown tracer")?;
        }

        rt.shutdown_timeout(std::time::Duration::from_secs(15));
        Ok(())
    }

    fn init() -> Result<()> {
        let init = Init::<DefaultInit>::default();
        init.unset_locale()?;
        init.set_default_umask();
        init.unset_notify_socket();
        init::save_oom_score();
        init::limit_malloc_arenas();
        // While we could configure this, standard practice has it as -1000,
        // so it may be YAGNI to add configuration.
        init.set_oom_score("-1000")
    }

    fn init_logging(&mut self) -> Result<()> {
        let level = LevelFilter::from(*self.config().log_level());

        #[cfg(feature = "tracing")]
        let telemetry_layer = if self.config().enable_tracing() {
            let tracer = Telemetry::layer(self.config().tracing_endpoint())
                .context("build telemetry layer")?;

            self.tracer = Some(tracer.clone());

            tracing_opentelemetry::layer()
                .with_tracer(tracer.tracer(env!("CARGO_PKG_NAME")))
                .into()
        } else {
            None
        };

        let registry = tracing_subscriber::registry();
        #[cfg(feature = "tracing")]
        let registry = registry.with(telemetry_layer);

        match self.config().log_driver() {
            LogDriver::None => {}
            LogDriver::Stdout => {
                let layer = tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_line_number(true)
                    .with_filter(level);
                registry
                    .with(layer)
                    .try_init()
                    .context("init stdout fmt layer")?;
                info!("Using stdout logger");
            }
            LogDriver::Systemd => {
                let layer = tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_line_number(true)
                    .without_time()
                    .with_writer(Journal::default())
                    .with_filter(level);
                registry
                    .with(layer)
                    .try_init()
                    .context("init journald fmt layer")?;
                info!("Using systemd/journald logger");
            }
        }
        info!("Set log level to: {}", self.config().log_level());
        Ok(())
    }

    /// Spawns all required tokio tasks and returns the tracer provider which
    /// `init_logging` created, so that it can be shut down after the runtime.
    async fn spawn_tasks(mut self) -> Result<Tracer> {
        self.init_logging().context("init logging")?;
        #[cfg(feature = "tracing")]
        let tracer = self.tracer.clone();
        #[cfg(not(feature = "tracing"))]
        let tracer = ();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let socket = self.config().socket();
        let fd_socket = self.config().fd_socket();
        let reaper = self.reaper.clone();
        self.reaper.clone().start_orphan_sweep();

        // Run both signal handler and backend inside spawn_blocking with LocalSet
        // This allows concurrent execution within the LocalSet while preventing
        // blocking of the main current_thread runtime
        let result = task::spawn_blocking(move || {
            Handle::current().block_on(async move {
                let local = LocalSet::new();

                // Spawn signal handler as a local task
                local.spawn_local(Self::traced(
                    Self::start_signal_handler(reaper, socket, fd_socket, shutdown_tx),
                    debug_span!("signal_handler"),
                ));

                // Run backend on the LocalSet
                local
                    .run_until(Self::traced(
                        self.start_backend(shutdown_rx),
                        debug_span!("backend"),
                    ))
                    .await
            })
        })
        .await?;
        result?;
        Ok(tracer)
    }

    /// Run the future inside the span, which also carries the OpenTelemetry
    /// context if the feature is enabled.
    #[cfg(feature = "tracing")]
    fn traced<F: Future>(future: F, span: Span) -> impl Future<Output = F::Output> {
        future.with_context(span.context()).instrument(span)
    }

    /// Run the future inside the span.
    #[cfg(not(feature = "tracing"))]
    fn traced<F: Future>(future: F, span: Span) -> impl Future<Output = F::Output> {
        future.instrument(span)
    }

    async fn start_signal_handler<T: AsRef<Path>>(
        reaper: Arc<ChildReaper>,
        socket: T,
        fd_socket: T,
        shutdown_tx: oneshot::Sender<()>,
    ) -> Result<()> {
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;

        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM");
            }
            _ = sigint.recv() => {
                info!("Received SIGINT");
            }
        }

        // Everything below has to happen before the shutdown message: the
        // backend returns on it and drops the local set, which cancels this
        // task. Failures are collected rather than returned early, the server
        // must stop in any case. The pause shutdown may wait for a running
        // initialization, the containers must not wait for that.
        let (result, ()) =
            tokio::join!(Self::cleanup(reaper, socket, fd_socket), Pause::shutdown());

        debug!("Sending shutdown message");
        shutdown_tx
            .send(())
            .map_err(|_| format_err!("unable to send shutdown message"))?;

        result
    }

    /// Kill the grandchildren and remove the socket files.
    async fn cleanup<T: AsRef<Path>>(
        reaper: Arc<ChildReaper>,
        socket: T,
        fd_socket: T,
    ) -> Result<()> {
        debug!("Starting grandchildren cleanup task");
        // Always use SIGKILL to ensure immediate termination of container processes
        let killed = reaper
            .kill_grandchildren(Signal::SIGKILL)
            .await
            .context("unable to kill grandchildren");

        debug!("Removing socket file {}", socket.as_ref().display());
        let socket_removed = fs::remove_file(socket)
            .await
            .context("remove existing socket file");

        debug!("Removing fd socket file {}", fd_socket.as_ref().display());
        let fd_socket_removed = fs::remove_file(fd_socket)
            .await
            .or_else(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(err)
                }
            })
            .context("remove existing fd socket file");

        killed.and(socket_removed).and(fd_socket_removed)
    }

    async fn start_backend(self, mut shutdown_rx: oneshot::Receiver<()>) -> Result<()> {
        let listener =
            Listener::<DefaultListener>::default().bind_long_path(self.config().socket())?;
        let client: conmon::Client = capnp_rpc::new_client(self);

        loop {
            let stream = tokio::select! {
                _ = &mut shutdown_rx => {
                    debug!("Received shutdown message");
                    return Ok(())
                }
                stream = listener.accept() => match stream {
                    Ok((stream, _)) => stream,
                    // One failed connection must not end the server.
                    Err(e) => {
                        listener::accept_failed(&e).await;
                        continue;
                    }
                },
            };
            let (reader, writer) = TokioAsyncReadCompatExt::compat(stream).split();
            let network = Box::new(VatNetwork::new(
                reader,
                writer,
                Side::Server,
                Default::default(),
            ));
            let rpc_system = RpcSystem::new(network, Some(client.clone().client));
            task::spawn_local(Box::pin(rpc_system.map(|_| ())));
        }
    }
}

pub(crate) struct GenerateRuntimeArgs<'a> {
    pub(crate) config: &'a Config,
    pub(crate) id: &'a str,
    pub(crate) container_io: &'a ContainerIO,
    pub(crate) pidfile: &'a Path,
    pub(crate) cgroup_manager: CgroupManager,
}

impl GenerateRuntimeArgs<'_> {
    const SYSTEMD_CGROUP_ARG: &'static str = "--systemd-cgroup";
    const RUNTIME_CRUN: &'static str = "crun";
    const LOG_LEVEL_FLAG_CRUN: &'static str = "--log-level";

    /// Generate the OCI runtime CLI arguments from the provided parameters.
    pub fn create_args(
        self,
        bundle_path: &Path,
        global_args: impl IntoIterator<Item = impl AsRef<str>>,
        command_args: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Vec<String>> {
        // Pre-allocate capacity for typical arg count to reduce reallocations
        let mut args = Vec::with_capacity(16);
        args.extend(self.default_args().context("build default runtime args")?);

        args.extend(global_args.into_iter().map(|arg| arg.as_ref().into()));

        // Use static strings where possible to avoid allocations
        args.push("create".into());
        args.push("--bundle".into());
        args.push(bundle_path.display().to_string());
        args.push("--pid-file".into());
        args.push(self.pidfile.display().to_string());

        args.extend(command_args.into_iter().map(|arg| arg.as_ref().into()));

        if let ContainerIOType::Terminal(terminal) = self.container_io.typ() {
            args.push(format!("--console-socket={}", terminal.path().display()));
        }

        args.push(self.id.into());

        debug!("Runtime args {:?}", args.join(" "));
        Ok(args)
    }

    /// Generate the OCI runtime CLI arguments from the provided parameters.
    pub(crate) fn exec_sync_args(
        &self,
        command: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Vec<String>> {
        let mut args = self
            .exec_sync_args_without_command()
            .context("exec sync args without command")?;

        args.extend(command.into_iter().map(|arg| arg.as_ref().into()));

        debug!("Exec args {:?}", args.join(" "));
        Ok(args)
    }

    pub(crate) fn exec_sync_args_without_command(&self) -> Result<Vec<String>> {
        // Pre-allocate capacity for typical arg count
        let mut args = Vec::with_capacity(12);
        args.extend(self.default_args().context("build default runtime args")?);

        // Use static strings to avoid allocations
        args.push("exec".into());
        args.push("-d".into());

        if let ContainerIOType::Terminal(terminal) = self.container_io.typ() {
            args.push(format!("--console-socket={}", terminal.path().display()));
            args.push("--tty".into());
        }

        args.push(format!("--pid-file={}", self.pidfile.display()));
        args.push(self.id.into());

        Ok(args)
    }

    /// Build the default arguments for any provided runtime.
    fn default_args(&self) -> Result<Vec<String>> {
        let mut args = vec![];

        if self
            .config
            .runtime()
            .file_name()
            .context("no filename in path")?
            == Self::RUNTIME_CRUN
        {
            debug!("Found crun used as runtime");
            args.push(format!("--log=journald:{}", self.id));

            match self.config.log_level() {
                &LogLevel::Debug | &LogLevel::Error => args.push(format!(
                    "{}={}",
                    Self::LOG_LEVEL_FLAG_CRUN,
                    self.config.log_level()
                )),
                &LogLevel::Warn => args.push(format!("{}=warning", Self::LOG_LEVEL_FLAG_CRUN)),
                _ => {}
            }
        }

        if let Some(rr) = self.config.runtime_root() {
            args.push(format!("--root={}", rr.display()));
        }

        if self.cgroup_manager == CgroupManager::Systemd {
            args.push(Self::SYSTEMD_CGROUP_ARG.into());
        }

        Ok(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_log::ContainerLog;
    use std::os::fd::AsRawFd;

    #[test]
    fn inherited_fds_reports_descriptors_above_stderr() -> Result<()> {
        let file = tempfile::tempfile()?;
        let fd = file.as_raw_fd();

        let fds = Server::inherited_fds();

        assert!(
            fds.contains(&fd),
            "expected an open descriptor {fd} to be reported, got {fds:?}"
        );
        // stdin, stdout and stderr are never reported, they are not ours to close.
        assert!(!fds.contains(&0));
        assert!(!fds.contains(&1));
        assert!(!fds.contains(&2));

        Ok(())
    }

    /// Build the arguments with a configuration from `flags`, None if the
    /// environment would change it.
    fn runtime_args<F>(flags: &[&str], cgroup_manager: CgroupManager, f: F) -> Option<Vec<String>>
    where
        F: FnOnce(GenerateRuntimeArgs) -> Result<Vec<String>>,
    {
        runtime_args_with_io(flags, cgroup_manager, false, f)
    }

    fn runtime_args_with_io<F>(
        flags: &[&str],
        cgroup_manager: CgroupManager,
        terminal: bool,
        f: F,
    ) -> Option<Vec<String>>
    where
        F: FnOnce(GenerateRuntimeArgs) -> Result<Vec<String>>,
    {
        let config = Config::parse_for_test(flags.iter().copied())?;
        let container_io =
            ContainerIO::new(terminal, ContainerLog::new()).expect("create container IO");
        let args = GenerateRuntimeArgs {
            config: &config,
            id: "id",
            container_io: &container_io,
            pidfile: Path::new("/pidfile"),
            cgroup_manager,
        };
        Some(f(args).expect("generate runtime args"))
    }

    fn create(args: GenerateRuntimeArgs) -> Result<Vec<String>> {
        args.create_args(Path::new("/bundle"), ["--global"], ["--command"])
    }

    #[test]
    fn create_args_for_runc() {
        let Some(args) = runtime_args(
            &["--runtime", "/usr/bin/runc"],
            CgroupManager::Cgroupfs,
            create,
        ) else {
            return;
        };
        assert_eq!(
            args,
            [
                "--global",
                "create",
                "--bundle",
                "/bundle",
                "--pid-file",
                "/pidfile",
                "--command",
                "id"
            ]
        );
    }

    #[test]
    fn create_args_for_crun_with_root_and_systemd() {
        let Some(args) = runtime_args(
            &[
                "--runtime",
                "/usr/bin/crun",
                "--runtime-root",
                "/run/crun",
                "--log-level",
                "debug",
            ],
            CgroupManager::Systemd,
            create,
        ) else {
            return;
        };
        assert_eq!(
            args,
            [
                "--log=journald:id",
                "--log-level=debug",
                "--root=/run/crun",
                "--systemd-cgroup",
                "--global",
                "create",
                "--bundle",
                "/bundle",
                "--pid-file",
                "/pidfile",
                "--command",
                "id"
            ]
        );
    }

    #[test]
    fn default_args_crun_log_levels() {
        // crun takes debug, warning and error, the other levels keep its
        // default.
        for (level, expected) in [
            ("trace", None),
            ("debug", Some("--log-level=debug")),
            ("info", None),
            ("warn", Some("--log-level=warning")),
            ("error", Some("--log-level=error")),
            ("off", None),
        ] {
            let Some(args) = runtime_args(
                &["--runtime", "/usr/bin/crun", "--log-level", level],
                CgroupManager::Cgroupfs,
                |args| args.default_args(),
            ) else {
                return;
            };
            let mut want = vec!["--log=journald:id"];
            want.extend(expected);
            assert_eq!(args, want, "level {level}");
        }

        // Other runtimes get neither the journald log nor the log level.
        let Some(args) = runtime_args(
            &["--runtime", "/usr/bin/runc", "--log-level", "debug"],
            CgroupManager::Cgroupfs,
            |args| args.default_args(),
        ) else {
            return;
        };
        assert!(args.is_empty());
    }

    #[test]
    fn exec_sync_args_without_terminal() {
        let Some(args) = runtime_args(
            &["--runtime", "/usr/bin/runc"],
            CgroupManager::Systemd,
            |args| args.exec_sync_args(["sh", "-c", "true"]),
        ) else {
            return;
        };
        assert_eq!(
            args,
            [
                "--systemd-cgroup",
                "exec",
                "-d",
                "--pid-file=/pidfile",
                "id",
                "sh",
                "-c",
                "true"
            ]
        );
    }

    #[tokio::test]
    async fn exec_and_create_args_with_terminal() {
        let mut socket = None;
        let Some(exec) = runtime_args_with_io(
            &["--runtime", "/usr/bin/runc"],
            CgroupManager::Cgroupfs,
            true,
            |args| {
                if let ContainerIOType::Terminal(terminal) = args.container_io.typ() {
                    socket = Some(terminal.path().display().to_string());
                }
                args.exec_sync_args(["true"])
            },
        ) else {
            return;
        };
        let console_socket = format!("--console-socket={}", socket.expect("terminal socket"));
        assert_eq!(
            exec,
            [
                "exec",
                "-d",
                console_socket.as_str(),
                "--tty",
                "--pid-file=/pidfile",
                "id",
                "true"
            ]
        );

        let mut socket = None;
        let Some(create) = runtime_args_with_io(
            &["--runtime", "/usr/bin/runc"],
            CgroupManager::Cgroupfs,
            true,
            |args| {
                if let ContainerIOType::Terminal(terminal) = args.container_io.typ() {
                    socket = Some(terminal.path().display().to_string());
                }
                create(args)
            },
        ) else {
            return;
        };
        let console_socket = format!("--console-socket={}", socket.expect("terminal socket"));
        // The console socket follows the command args, the ID comes last.
        assert_eq!(
            create,
            [
                "--global",
                "create",
                "--bundle",
                "/bundle",
                "--pid-file",
                "/pidfile",
                "--command",
                console_socket.as_str(),
                "id"
            ]
        );
    }

    #[tokio::test]
    async fn cleanup_removes_the_sockets() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("conmon.sock");
        let fd_socket = dir.path().join("conmon-fd.sock");
        File::create(&socket)?;
        File::create(&fd_socket)?;

        let reaper = Arc::new(ChildReaper::default());
        Server::cleanup(reaper.clone(), &socket, &fd_socket).await?;
        assert!(!socket.exists());
        assert!(!fd_socket.exists());

        // An fd socket which was never started is fine, a missing main
        // socket is reported.
        File::create(&socket)?;
        Server::cleanup(reaper.clone(), &socket, &fd_socket).await?;
        assert!(Server::cleanup(reaper, &socket, &fd_socket).await.is_err());
        Ok(())
    }
}
