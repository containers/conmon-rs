//! The server process: its start and fork, its logging, the RPC socket it
//! serves and its shutdown on a signal.

use crate::conmon_capnp::conmon::{self};
#[cfg(feature = "tracing")]
use crate::telemetry::Telemetry;
use crate::{
    child_reaper::ChildReaper,
    config::{Config, LogDriver},
    fd_socket::FdSocket,
    fs_util, init,
    journal::Journal,
    listener,
    pause::Pause,
    streaming_server::StreamingServer,
};
use anyhow::{Context, Result, format_err};
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp::Side, twoparty};
use futures::{
    AsyncReadExt, FutureExt,
    io::{BufReader, BufWriter},
};
use libc::_exit;
use nix::{
    sys::{
        prctl,
        signal::{Signal, kill},
    },
    unistd::{ForkResult, chdir, fork},
};
#[cfg(feature = "tracing")]
use opentelemetry::trace::{FutureExt as OpenTelemetryFutureExt, TracerProvider};
#[cfg(feature = "tracing")]
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::{fs::File, io::Write, os::fd::RawFd, path::Path, sync::Arc};
use tokio::{
    fs,
    net::UnixListener,
    runtime::Builder,
    signal::unix::{Signal as SignalStream, SignalKind, signal},
    sync::{RwLock, oneshot},
    task::{self, LocalSet},
    time::{self, Duration, Instant},
};
use tokio_util::compat::TokioAsyncReadCompatExt;
#[cfg(feature = "tracing")]
use tracing::warn;
use tracing::{Instrument, Span, debug, debug_span, error, info};
#[cfg(feature = "tracing")]
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, prelude::*};
use twoparty::VatNetwork;

/// The tracer provider created by `init_logging`, if the feature is enabled.
#[cfg(feature = "tracing")]
type Tracer = Option<SdkTracerProvider>;
#[cfg(not(feature = "tracing"))]
type Tracer = ();

/// How long the clients wait for the server to exit on a shutdown before
/// they kill it, `serverStopTimeout` in `pkg/client/server.go`.
const CLIENT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

// The whole shutdown ends before the clients kill the server.
const _: () = assert!(Server::SHUTDOWN_TIMEOUT.as_nanos() < CLIENT_STOP_TIMEOUT.as_nanos());

// The cleanup gets what the tracer and the runtime leave of the shutdown.
// That leaves the grandchildren and a running initialization of the pause
// instance time after what the pause shutdown and the sockets take.
const _: () = assert!(
    Server::CLEANUP_TIMEOUT.as_nanos()
        > Server::SOCKET_REMOVAL_TIMEOUT.as_nanos()
            + Pause::KILL_WAIT.as_nanos()
            + Pause::STOP_WAIT.as_nanos()
);

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
        Ok(Self::with_config(config))
    }

    /// A server for the configuration without the setup of the process,
    /// which tests must not change.
    pub(crate) fn with_config(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            reaper: Default::default(),
            fd_socket: Default::default(),
            #[cfg(feature = "tracing")]
            tracer: Default::default(),
            streaming_server: Default::default(),
        }
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
    #[expect(unsafe_code, reason = "closing descriptors which may be gone")]
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
                // SAFETY: as above.
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
            );
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

    /// Upper bound for the threads of the blocking pool, which serve file
    /// I/O like log writes, exit files and journald. Without a bound a burst
    /// of container exits or log reopens spawns a thread per operation
    /// (tokio's default allows 512), each with its own stack. Work which
    /// blocks for an unbounded time, like waiting for a process, must run on
    /// a dedicated thread instead, otherwise it can starve the pool.
    const MAX_BLOCKING_THREADS: usize = 16;

    /// Stack size of the threads of the blocking pool, which only run short
    /// file and socket operations. The shutdown of the tracer, which exports
    /// the remaining spans, runs on one as well.
    const BLOCKING_THREAD_STACK_SIZE: usize = 512 * 1024;

    /// Start the `Server` instance and consume it.
    #[expect(unsafe_code, reason = "forking")]
    pub fn start(self) -> Result<()> {
        // Fork as early as possible, before setting up tokio: a child of a
        // process with threads may only call async-signal-safe functions
        // until it execs, see the safety section of `nix::unistd::fork`.
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
                        // SAFETY: _exit(2) ends the process right away, the
                        // parent has nothing to flush or clean up.
                        unsafe { _exit(1) };
                    }
                    // SAFETY: as above.
                    unsafe { _exit(0) };
                }
                ForkResult::Child => {
                    // Like a daemon, do not keep the standard streams of the
                    // caller, like the file the Go client reads the launch
                    // errors from, see `startServer` in `pkg/client`. The
                    // processes the server starts, like cleanup commands,
                    // would write into it otherwise. Only the stdout log
                    // driver writes to them.
                    if self.config().log_driver() != LogDriver::Stdout {
                        use nix::{
                            fcntl::{OFlag, open},
                            sys::stat::Mode,
                            unistd::{dup2_stderr, dup2_stdin, dup2_stdout},
                        };
                        use std::os::fd::{AsRawFd, IntoRawFd};

                        let null = open(c"/dev/null", OFlag::O_RDWR, Mode::empty())
                            .context("open /dev/null")?;
                        dup2_stdin(&null).context("redirect stdin")?;
                        dup2_stdout(&null).context("redirect stdout")?;
                        dup2_stderr(&null).context("redirect stderr")?;
                        // A closed standard stream got it, which has to stay.
                        if null.as_raw_fd() <= libc::STDERR_FILENO {
                            let _: RawFd = null.into_raw_fd();
                        }
                    }
                }
            }
        }

        // Deliberately outside the `skip_fork` check above: the server outlives
        // its parent either way, so the descriptors have to go either way.
        Self::close_inherited_fds();

        // Do not keep the working directory of the caller busy, like a mount
        // which then cannot be unmounted. The paths of the configuration are
        // absolute, see `Config::make_absolute`, and so are the ones of the
        // requests.
        chdir("/").context("change into the root directory")?;

        // The container processes get reparented to the server once their
        // runtime exits.
        prctl::set_child_subreaper(true).context("set child subreaper")?;

        let rt = Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .max_blocking_threads(Self::MAX_BLOCKING_THREADS)
            .thread_stack_size(Self::BLOCKING_THREAD_STACK_SIZE)
            .build()?;
        // Nothing returns early from here on: a thread of the blocking pool
        // which is stuck, like on a hung file system, ends with the process.
        // Dropping the runtime would wait for it instead, so it gets shut
        // down with a timeout however the server ends.
        let (tracer, result) = rt.block_on(self.spawn_tasks());
        #[cfg(feature = "tracing")]
        let result = result.and(Self::shutdown_tracer(&rt, tracer));
        #[cfg(not(feature = "tracing"))]
        let () = tracer;
        rt.shutdown_timeout(Self::RUNTIME_SHUTDOWN_TIMEOUT);
        result
    }

    /// Export the remaining spans. That needs the runtime the exporter was
    /// created on, so shut down from a blocking task while it is driven.
    #[cfg(feature = "tracing")]
    fn shutdown_tracer(rt: &tokio::runtime::Runtime, tracer: Tracer) -> Result<()> {
        let Some(tracer) = tracer else {
            return Ok(());
        };
        let shutdown = rt.block_on(async {
            time::timeout(
                Self::TRACER_SHUTDOWN_TIMEOUT,
                task::spawn_blocking(move || tracer.shutdown()),
            )
            .await
        });
        if let Ok(res) = shutdown {
            res.context("join tracer shutdown")?
                .context("shutdown tracer")
        } else {
            warn!("Tracer did not shut down in time");
            Ok(())
        }
    }

    /// How long the shutdown may take from the signal on, below the
    /// [`CLIENT_STOP_TIMEOUT`] of the clients. Every part of the shutdown
    /// gets its time from it.
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(9500);

    /// How long the tracer may take to export the remaining spans, at the
    /// end of the shutdown.
    const TRACER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

    /// How long the runtime waits for the threads of the blocking pool, at
    /// the very end of the shutdown.
    const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

    /// How long the cleanup may take: the grandchildren, the pause instance
    /// and the sockets, whatever the tracer and the runtime leave.
    const CLEANUP_TIMEOUT: Duration = Self::SHUTDOWN_TIMEOUT
        .saturating_sub(Self::TRACER_SHUTDOWN_TIMEOUT)
        .saturating_sub(Self::RUNTIME_SHUTDOWN_TIMEOUT);

    /// How long the removal of the sockets may take at the end of the
    /// cleanup, through a blocking pool which stuck threads may fill up.
    const SOCKET_REMOVAL_TIMEOUT: Duration = Duration::from_millis(500);

    fn init() -> Result<()> {
        // The server keeps the C locale, unlike conmon, so that the errors
        // it returns to CRI-O do not depend on the environment.
        init::set_default_umask();
        init::unset_notify_socket();
        init::save_oom_score();
        init::raise_nofile_limit();
        init::configure_malloc();
        // While we could configure this, standard practice has it as -1000,
        // so it may be YAGNI to add configuration.
        init::set_oom_score("-1000")
    }

    #[cfg_attr(
        not(feature = "tracing"),
        expect(
            clippy::needless_pass_by_ref_mut,
            reason = "keeps the tracer with the tracing feature"
        )
    )]
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
            LogDriver::None => {
                // Without logs the spans still get exported. A subscriber
                // without any layer would only cost.
                #[cfg(feature = "tracing")]
                if self.tracer.is_some() {
                    registry.try_init().context("init telemetry layer")?;
                }
            }
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

    /// Spawns all required tokio tasks until the server ends, and returns
    /// the tracer provider which `init_logging` created along with the
    /// result, so that it can be shut down after the runtime also on a
    /// failure.
    async fn spawn_tasks(mut self) -> (Tracer, Result<()>) {
        let logging = self.init_logging().context("init logging");
        #[cfg(feature = "tracing")]
        let tracer = self.tracer.clone();
        #[cfg(not(feature = "tracing"))]
        let tracer = ();
        if let Err(e) = logging {
            return (tracer, Err(e));
        }
        (tracer, self.run_tasks().await)
    }

    /// Run the backend and the signal handler until the server ends.
    async fn run_tasks(self) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let socket = self.config().socket();
        let fd_socket = self.config().fd_socket();
        let reaper = self.reaper.clone();
        self.reaper.clone().start_orphan_sweep();
        // Before the backend binds the socket, a signal which arrives once a
        // client can connect must shut the server down, not kill it.
        let signals = Self::listen_for_signals();

        // The RPC system is not `Send`, so the backend and the tasks of the
        // calls run on a local set. It runs on the thread of the runtime
        // itself, which drives the I/O and the timers of both, and the
        // signal handler next to it.
        let local = LocalSet::new();
        local.spawn_local(Self::traced(
            Self::start_signal_handler(signals, reaper, socket, fd_socket, shutdown_tx),
            debug_span!("signal_handler"),
        ));
        local
            .run_until(Self::traced(
                self.start_backend(shutdown_rx),
                debug_span!("backend"),
            ))
            .await
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

    /// Wait for SIGTERM or SIGINT and clean up, then end the backend with
    /// the result, which the server exits with. A server which cannot listen
    /// for the signals ends right away, after the same cleanup.
    async fn start_signal_handler<T: AsRef<Path>>(
        signals: Result<[SignalStream; 2]>,
        reaper: Arc<ChildReaper>,
        socket: T,
        fd_socket: T,
        shutdown_tx: oneshot::Sender<Result<()>>,
    ) {
        let signalled = Self::wait_for_signal(signals).await;
        let shutdown = Self::shutdown(reaper, socket, fd_socket, Pause::shutdown).await;
        let result = Self::all_ok([signalled, shutdown]);
        // The backend returns it, the server exits non-zero on a failure.
        debug!("Sending shutdown message");
        if shutdown_tx.send(result).is_err() {
            debug!("Backend already ended");
        }
    }

    /// Listen for SIGTERM and SIGINT, which shut the server down.
    fn listen_for_signals() -> Result<[SignalStream; 2]> {
        Ok([
            signal(SignalKind::terminate()).context("listen for SIGTERM")?,
            signal(SignalKind::interrupt()).context("listen for SIGINT")?,
        ])
    }

    /// Wait for SIGTERM or SIGINT, see [`Server::listen_for_signals`].
    async fn wait_for_signal(signals: Result<[SignalStream; 2]>) -> Result<()> {
        let [mut sigterm, mut sigint] = signals?;

        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM");
            }
            _ = sigint.recv() => {
                info!("Received SIGINT");
            }
        }
        Ok(())
    }

    /// Clean up and stop the pause instance through `pause` within the
    /// [`Server::CLEANUP_TIMEOUT`], and report what did not finish.
    async fn shutdown<T: AsRef<Path>, F: Future<Output = Result<()>>>(
        reaper: Arc<ChildReaper>,
        socket: T,
        fd_socket: T,
        pause: impl FnOnce(Instant) -> F,
    ) -> Result<()> {
        // The pause shutdown may wait for a running initialization, the
        // containers must not wait for that.
        let deadline = Instant::now() + Self::CLEANUP_TIMEOUT;
        let (cleaned, paused) = tokio::join!(
            Self::cleanup(reaper, socket, fd_socket, deadline),
            Self::until(deadline, pause(deadline)),
        );
        let result = Self::all_ok([cleaned, paused.context("stop pause")]);
        if let Err(e) = &result {
            error!("Shutdown did not finish: {e:#}");
        }
        result
    }

    /// The result of the server once the signal handler sent the shutdown
    /// message, see [`Server::start_signal_handler`].
    fn shutdown_result(received: Result<Result<()>, oneshot::error::RecvError>) -> Result<()> {
        received.context("signal handler ended without a result")?
    }

    /// Kill the grandchildren and remove the socket files, until the
    /// `deadline`. Failures are collected rather than returned early, the
    /// server must stop in any case.
    async fn cleanup<T: AsRef<Path>>(
        reaper: Arc<ChildReaper>,
        socket: T,
        fd_socket: T,
        deadline: Instant,
    ) -> Result<()> {
        debug!("Starting grandchildren cleanup task");
        let reaper_deadline = deadline
            .checked_sub(Self::SOCKET_REMOVAL_TIMEOUT)
            .unwrap_or(deadline);
        let killed = Self::until(deadline, reaper.kill_grandchildren(reaper_deadline))
            .await
            .context("kill grandchildren");

        debug!("Removing socket file {}", socket.as_ref().display());
        let socket_removed = Self::until(deadline, async {
            fs_util::removed(fs::remove_file(socket).await).map_err(Into::into)
        })
        .await
        .context("remove socket file");

        debug!("Removing fd socket file {}", fd_socket.as_ref().display());
        let fd_socket_removed = Self::until(deadline, async {
            fs_util::removed(fs::remove_file(fd_socket).await).map_err(Into::into)
        })
        .await
        .context("remove fd socket file");

        Self::all_ok([killed, socket_removed, fd_socket_removed])
    }

    /// Run the future until the deadline, fail if it did not finish by then.
    async fn until(deadline: Instant, future: impl Future<Output = Result<()>>) -> Result<()> {
        time::timeout_at(deadline, future)
            .await
            .unwrap_or_else(|_| Err(format_err!("did not finish in time")))
    }

    /// Succeed if all succeeded, fail with every failure otherwise.
    fn all_ok(results: impl IntoIterator<Item = Result<()>>) -> Result<()> {
        let failures: Vec<_> = results
            .into_iter()
            .filter_map(Result::err)
            .map(|e| format!("{e:#}"))
            .collect();
        if failures.is_empty() {
            return Ok(());
        }
        Err(format_err!("{}", failures.join(", ")))
    }

    /// The read and write buffers of an RPC connection, which most messages
    /// fit into. Larger segments bypass them.
    const RPC_BUFFER_SIZE: usize = 4096;

    async fn start_backend(self, mut shutdown_rx: oneshot::Receiver<Result<()>>) -> Result<()> {
        let listener = listener::bind_long_path(&self.config().socket(), UnixListener::bind)?;
        let client: conmon::Client = capnp_rpc::new_client(self);
        let mut accept_errors = listener::AcceptErrors::default();

        loop {
            let stream = tokio::select! {
                result = &mut shutdown_rx => {
                    debug!("Received shutdown message");
                    return Self::shutdown_result(result);
                }
                stream = listener.accept() => match stream {
                    Ok((stream, _)) => stream,
                    // One failed connection must not end the server.
                    Err(e) => {
                        accept_errors.failed(&e).await;
                        continue;
                    }
                },
            };
            // A message gets read and written in several pieces, the segment
            // table and every segment on its own.
            let (reader, writer) = TokioAsyncReadCompatExt::compat(stream).split();
            let network = Box::new(VatNetwork::new(
                BufReader::with_capacity(Self::RPC_BUFFER_SIZE, reader),
                BufWriter::with_capacity(Self::RPC_BUFFER_SIZE, writer),
                Side::Server,
                Default::default(),
            ));
            let rpc_system = RpcSystem::new(network, Some(client.clone().client));
            task::spawn_local(Box::pin(rpc_system.map(|_| ())));
        }
    }
}

#[cfg(test)]
mod tests;
