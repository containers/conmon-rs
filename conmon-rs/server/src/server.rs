//! The server process: its start and fork, its logging, the RPC socket it
//! serves and its shutdown on a signal.

use crate::conmon_capnp::conmon::{self};
#[cfg(feature = "tracing")]
use crate::telemetry;
use crate::{
    child_reaper::ChildReaper,
    config::{Config, LogDriver},
    container_io::DRAIN_MAX_WAIT,
    init,
    journal::{self, Journal},
    listener,
    streaming_server::StreamingServer,
};
use anyhow::{Context, Result, format_err};
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp::Side, twoparty};
use futures::{
    AsyncReadExt, FutureExt, TryFutureExt,
    io::{BufReader, BufWriter},
};
use nix::{
    sys::signal::{Signal, kill},
    unistd::{ForkResult, Pid, fork},
};
#[cfg(feature = "tracing")]
use opentelemetry::trace::{FutureExt as OpenTelemetryFutureExt, TracerProvider};
#[cfg(feature = "tracing")]
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::{
    fs::File,
    io::{ErrorKind, Write},
    path::Path,
    process,
    sync::Arc,
};
use tokio::{
    fs,
    net::UnixListener,
    runtime::{Builder, Runtime},
    signal::unix::{Signal as SignalStream, SignalKind, signal},
    sync::oneshot,
    task::{self, LocalSet},
    time::{self, Duration, Instant},
};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{Instrument, Span, debug, debug_span, error, info, warn};
#[cfg(feature = "tracing")]
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, prelude::*};
use twoparty::VatNetwork;

/// How long the clients wait for the server to exit on a shutdown before
/// they kill it, `serverStopTimeout` in `pkg/client/server.go`.
const CLIENT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

// The whole shutdown ends before the clients kill the server.
const _: () = assert!(Server::SHUTDOWN_TIMEOUT.as_nanos() < CLIENT_STOP_TIMEOUT.as_nanos());

// The cleanup gets what the tracer and the runtime leave of the shutdown.
// That leaves the killed grandchildren time to drain their output after the
// grace of the containers on SIGTERM and what the socket takes. It does not
// cover the DRAIN_MAX_LOG_WAIT for a slow log driver, which the watchers of
// the containers wait for as well: one which needs longer than the rest of
// the deadline gets reported as not exited, and the server may stop before
// it wrote the exit file.
const _: () = assert!(
    Server::CLEANUP_TIMEOUT.as_nanos()
        > Server::TERMINATE_GRACE.as_nanos()
            + Server::SOCKET_REMOVAL_TIMEOUT.as_nanos()
            + DRAIN_MAX_WAIT.as_nanos()
);

// The runtime calls in progress get stopped after the grace of the
// containers on SIGTERM, and the stopped ones get to clean up before the
// deadline of the grandchildren, which leaves the time of the socket removal.
const _: () = assert!(
    Server::CLEANUP_TIMEOUT.as_nanos()
        > Server::TERMINATE_GRACE.as_nanos()
            + Server::SOCKET_REMOVAL_TIMEOUT.as_nanos()
            + ChildReaper::STOP_BUDGET.as_nanos()
);

/// The conmonrs process: it serves the RPCs of its configuration and holds
/// what the calls share, the reaper of the containers and execs and the
/// streaming server.
#[derive(Debug)]
pub(crate) struct Server {
    config: Arc<Config>,
    reaper: Arc<ChildReaper>,

    /// The tracer provider which exports the spans, if tracing is enabled.
    #[cfg(feature = "tracing")]
    tracer: Option<SdkTracerProvider>,
    streaming_server: Arc<StreamingServer>,
}

impl Server {
    /// The configuration of the server.
    pub(crate) fn config(&self) -> &Arc<Config> {
        &self.config
    }

    /// The reaper of the containers and execs.
    pub(crate) fn reaper(&self) -> &Arc<ChildReaper> {
        &self.reaper
    }

    /// The streaming server of the exec, attach and port forward sessions.
    pub(crate) fn streaming_server(&self) -> &Arc<StreamingServer> {
        &self.streaming_server
    }

    /// Create a new server for a validated and prepared configuration.
    pub(crate) fn new(config: Config) -> Result<Self> {
        init::set_up_process().context("set up process")?;
        Ok(Self::with_config(config))
    }

    /// A server for the configuration without the setup of the process,
    /// which tests must not change.
    pub(crate) fn with_config(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            reaper: Default::default(),
            #[cfg(feature = "tracing")]
            tracer: Default::default(),
            streaming_server: Default::default(),
        }
    }

    /// Upper bound for the threads of the blocking pool, which serve file
    /// I/O like log writes, exit files and journald. Without a bound a burst
    /// of container exits or log reopens spawns a thread per operation
    /// (tokio's default allows 512), each with its own stack. Work which
    /// blocks for an unbounded time, like waiting for a process, must run on
    /// a dedicated thread instead, or be capped, like
    /// [`ContainerLog::sync`](crate::container_log::ContainerLog::sync),
    /// otherwise it can starve the pool.
    pub(crate) const MAX_BLOCKING_THREADS: usize = 16;

    /// Stack size of the threads of the blocking pool, which only run short
    /// file and socket operations. The shutdown of the tracer, which exports
    /// the remaining spans, runs on one as well.
    const BLOCKING_THREAD_STACK_SIZE: usize = 512 * 1024;

    /// Start the `Server` instance and consume it.
    #[expect(unsafe_code, reason = "forking")]
    pub(crate) fn start(mut self) -> Result<()> {
        // Fork as early as possible, before setting up tokio: a child of a
        // process with threads may only call async-signal-safe functions
        // until it execs, see the safety section of `nix::unistd::fork`.
        if !self.config().skip_fork() {
            // SAFETY: nothing else runs yet, the tokio runtime is built only
            // after the fork, so the child does not inherit a half initialized
            // thread pool. The parent only writes the PID file and exits.
            match unsafe { fork()? } {
                ForkResult::Parent { child, .. } => self.exit_parent(child),
                ForkResult::Child => init::redirect_stdio(self.config().log_driver())
                    .map_err(|e| self.log_early(e))?,
            }
            // The default hook prints the message of a panic to stderr,
            // which goes nowhere now with a driver other than stdout. Also
            // with `none`, the default of CRI-O: a panic aborts the server,
            // its containers lose their monitor, and the message is the only
            // clue. Without the fork, stderr is kept, under a systemd unit
            // it is the journal already.
            if self.config().log_driver() != LogDriver::Stdout {
                journal::log_panics();
            }
        }

        // Deliberately outside the `skip_fork` check above: the server outlives
        // its parent either way, so the descriptors have to go either way.
        init::close_inherited_fds();

        let rt = match Self::build_runtime() {
            Ok(rt) => rt,
            Err(e) => return Err(self.log_early(e)),
        };
        // Nothing returns early from here on: a thread of the blocking pool
        // which is stuck, like on a hung file system, ends with the process.
        // Dropping the runtime would wait for it instead, so it gets shut
        // down with a timeout however the server ends. The logging comes
        // first, so that the errors which follow get logged. With a log
        // driver other than `stdout`, the standard streams are gone after
        // the fork. The exporter of the tracer needs the runtime.
        let logging = rt
            .block_on(async { self.init_logging() })
            .context("init logging");
        #[cfg(feature = "tracing")]
        let tracer = self.tracer.clone();
        let result = match logging {
            Ok(()) => rt.block_on(self.serve()),
            Err(e) => Err(self.log_early(e)),
        };
        #[cfg(feature = "tracing")]
        let result = result.and(Self::shutdown_tracer(&rt, tracer));
        rt.shutdown_timeout(Self::RUNTIME_SHUTDOWN_TIMEOUT);
        result
    }

    /// Write the PID of the forked `child` to the PID file and exit, which
    /// is all the parent does. It is the process which got started, still
    /// with its only thread, so it can exit the usual way: nothing but the
    /// profile runtime of a coverage build registered an exit handler, stdout
    /// holds no output, and the destructors of the thread locals, which the
    /// libc may run, only free memory.
    fn exit_parent(&self, child: Pid) -> ! {
        let pidfile = self.config().conmon_pidfile();
        if let Err(e) = File::create(&pidfile).and_then(|mut f| write!(f, "{child}")) {
            // Without the PID file nobody knows the server, do not leave it
            // running behind a failure.
            eprintln!("Unable to write PID file {}: {e}", pidfile.display());
            let _ = kill(child, Signal::SIGKILL);
            process::exit(1);
        }
        process::exit(0);
    }

    /// The runtime of the server, see [`Server::MAX_BLOCKING_THREADS`].
    fn build_runtime() -> Result<Runtime> {
        Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .max_blocking_threads(Self::MAX_BLOCKING_THREADS)
            .thread_stack_size(Self::BLOCKING_THREAD_STACK_SIZE)
            .build()
            .context("build runtime")
    }

    /// Log an error from before the logging got set up, and return it. The
    /// systemd driver still gets it into the journal, the standard streams
    /// are gone after the fork, see [`init::redirect_stdio`]. With the
    /// stdout driver, `main` prints it to the stderr of the caller.
    fn log_early(&self, e: anyhow::Error) -> anyhow::Error {
        if self.config().log_driver() == LogDriver::Systemd
            && let Err(journal_error) = Journal::error().write_all(format!("{e:#}").as_bytes())
        {
            // Only seen while the stderr of the caller is kept, like without
            // the fork.
            eprintln!("Unable to log to the journal: {journal_error}");
        }
        e
    }

    /// Set up the process to serve, once the logging is, and serve until the
    /// server ends. Its failures get logged, also the one of the backend, see
    /// [`Server::logged`].
    async fn serve(self) -> Result<()> {
        let result = match init::set_up_serving() {
            Ok(()) => self.run_tasks().await,
            Err(e) => Err(e),
        };
        Self::logged(result)
    }

    /// The result of the server, logged unless the shutdown logged it
    /// already: the outer one is a failure of the server itself, the inner
    /// one the result of the shutdown, see [`Server::start_signal_handler`].
    fn logged(result: Result<Result<()>>) -> Result<()> {
        result.unwrap_or_else(|e| {
            error!("{e:#}");
            Err(e)
        })
    }

    /// Export the remaining spans. That needs the runtime the exporter was
    /// created on, so shut down from a blocking task while it is driven.
    #[cfg(feature = "tracing")]
    fn shutdown_tracer(rt: &Runtime, tracer: Option<SdkTracerProvider>) -> Result<()> {
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
        let Ok(res) = shutdown else {
            warn!("Tracer did not shut down in time");
            return Ok(());
        };
        let res = res
            .context("join tracer shutdown")
            .and_then(|res| res.context("shutdown tracer"));
        if let Err(e) = &res {
            error!("{e:#}");
        }
        res
    }

    /// How long the shutdown may take from the signal on, below the
    /// [`CLIENT_STOP_TIMEOUT`] of the clients, which send SIGINT. After
    /// SIGTERM, the [`Server::TERMINATE_GRACE`] is part of it. Every part of
    /// the shutdown gets its time from it.
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(9500);

    /// How long the tracer may take to export the remaining spans, at the
    /// end of the shutdown.
    const TRACER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

    /// How long the runtime waits for the threads of the blocking pool, at
    /// the very end of the shutdown.
    const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

    /// How long the cleanup may take: the grandchildren and the socket,
    /// whatever the tracer and the runtime leave.
    const CLEANUP_TIMEOUT: Duration = Self::SHUTDOWN_TIMEOUT
        .saturating_sub(Self::TRACER_SHUTDOWN_TIMEOUT)
        .saturating_sub(Self::RUNTIME_SHUTDOWN_TIMEOUT);

    /// How long the removal of the socket may take at the end of the
    /// cleanup, through a blocking pool which stuck threads may fill up.
    const SOCKET_REMOVAL_TIMEOUT: Duration = Duration::from_millis(500);

    #[cfg_attr(
        not(feature = "tracing"),
        expect(
            clippy::needless_pass_by_ref_mut,
            reason = "keeps the tracer with the tracing feature"
        )
    )]
    fn init_logging(&mut self) -> Result<()> {
        let level = LevelFilter::from(self.config().log_level());

        #[cfg(feature = "tracing")]
        let telemetry_layer = if self.config().enable_tracing() {
            let tracer = telemetry::tracer_provider(self.config().tracing_endpoint())
                .context("build tracer provider")?;

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
        let level = self.config().log_level();
        info!("Set log level to: {level}");
        // The client passes the flag along from the configuration of its
        // user, who would wait for traces which never come otherwise.
        #[cfg(not(feature = "tracing"))]
        if self.config().enable_tracing() {
            warn!("Ignoring --enable-tracing, this build has no tracing feature");
        }
        Ok(())
    }

    /// Run the backend and the signal handler until the server ends, see
    /// [`Server::logged`] for the result.
    async fn run_tasks(self) -> Result<Result<()>> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let socket = self.config().socket();
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
            Self::start_signal_handler(signals, reaper, socket, shutdown_tx),
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

    /// Wait for a signal which shuts the server down and clean up, then end
    /// the backend with the result, which the server exits with. A server
    /// which cannot listen for the signals ends right away, after the same
    /// cleanup.
    async fn start_signal_handler<T: AsRef<Path>>(
        signals: Result<[SignalStream; 3]>,
        reaper: Arc<ChildReaper>,
        socket: T,
        shutdown_tx: oneshot::Sender<Result<()>>,
    ) {
        let signalled = Self::wait_for_signal(signals, &reaper).await;
        let deadline = match &signalled {
            Ok(deadline) => *deadline,
            Err(e) => {
                error!("Unable to wait for a signal, shutting down: {e:#}");
                Instant::now() + Self::CLEANUP_TIMEOUT
            }
        };
        let shutdown = Self::shutdown(reaper, socket, deadline).await;
        let result = Self::all_ok([signalled.map(drop), shutdown]);
        // The backend returns it, the server exits non-zero on a failure.
        debug!("Sending shutdown message");
        if shutdown_tx.send(result).is_err() {
            debug!("Backend already ended");
        }
    }

    /// Listen for SIGTERM, SIGINT and SIGQUIT, which shut the server down.
    /// SIGQUIT like conmon, whose default action would end the server
    /// without the cleanup and leave its containers without a monitor. The
    /// other signals keep their default action.
    fn listen_for_signals() -> Result<[SignalStream; 3]> {
        Ok([
            signal(SignalKind::terminate()).context("listen for SIGTERM")?,
            signal(SignalKind::interrupt()).context("listen for SIGINT")?,
            signal(SignalKind::quit()).context("listen for SIGQUIT")?,
        ])
    }

    /// Wait for SIGTERM, SIGINT or SIGQUIT, see
    /// [`Server::listen_for_signals`], and return the deadline of the
    /// cleanup, counted from the signal. On SIGTERM, the containers get the
    /// [`Server::TERMINATE_GRACE`] of it to exit on it first, unless another
    /// SIGTERM, a SIGINT or a SIGQUIT asks for the shutdown meanwhile.
    async fn wait_for_signal(
        signals: Result<[SignalStream; 3]>,
        reaper: &ChildReaper,
    ) -> Result<Instant> {
        let [mut sigterm, mut sigint, mut sigquit] = signals?;

        let terminated = tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM");
                true
            }
            _ = sigint.recv() => {
                info!("Received SIGINT");
                false
            }
            _ = sigquit.recv() => {
                info!("Received SIGQUIT");
                false
            }
        };
        let start = Instant::now();
        if terminated {
            tokio::select! {
                () = reaper.terminate_containers(start + Self::TERMINATE_GRACE) => {}
                _ = sigterm.recv() => {
                    info!("Received another SIGTERM, not waiting for the containers");
                }
                _ = sigint.recv() => {
                    info!("Received SIGINT, not waiting for the containers");
                }
                _ = sigquit.recv() => {
                    info!("Received SIGQUIT, not waiting for the containers");
                }
            }
        }
        Ok(start + Self::CLEANUP_TIMEOUT)
    }

    /// How long the containers may take to exit on SIGTERM before they get
    /// killed, as part of the [`Server::CLEANUP_TIMEOUT`], so that SIGTERM
    /// does not make the shutdown take longer. Those which exited wrote
    /// their exit files by then. The rest of the cleanup kills the others,
    /// whose watchers then drain their output within [`DRAIN_MAX_WAIT`],
    /// unless a log driver is slow, which they wait for up to
    /// [`DRAIN_MAX_LOG_WAIT`](crate::container_io::DRAIN_MAX_LOG_WAIT),
    /// more than the rest of the cleanup leaves.
    const TERMINATE_GRACE: Duration = Duration::from_secs(5);

    /// Clean up until the `deadline`, and report what did not finish.
    async fn shutdown<T: AsRef<Path>>(
        reaper: Arc<ChildReaper>,
        socket: T,
        deadline: Instant,
    ) -> Result<()> {
        let result = Self::cleanup(reaper, socket, deadline).await;
        if let Err(e) = &result {
            error!("Shutdown did not finish: {e:#}");
        }
        result
    }

    /// The result of the server once the signal handler sent the shutdown
    /// message, see [`Server::start_signal_handler`], which logged it.
    fn shutdown_result(
        received: Result<Result<()>, oneshot::error::RecvError>,
    ) -> Result<Result<()>> {
        received.context("signal handler ended without a result")
    }

    /// Kill the grandchildren and remove the socket file, until the
    /// `deadline`. Failures are collected rather than returned early, the
    /// server must stop in any case.
    async fn cleanup<T: AsRef<Path>>(
        reaper: Arc<ChildReaper>,
        socket: T,
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
            // One which got removed already, like with its directory, is
            // fine.
            match fs::remove_file(socket).await {
                Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            }
        })
        .await
        .context("remove socket file");

        Self::all_ok([killed, socket_removed])
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

    /// Serve the RPC socket until the shutdown message, see
    /// [`Server::logged`] for the result.
    async fn start_backend(
        self,
        mut shutdown_rx: oneshot::Receiver<Result<()>>,
    ) -> Result<Result<()>> {
        let socket = self.config().socket();
        // Not with_context, which the FutureExt of OpenTelemetry makes
        // ambiguous with the tracing feature.
        let listener = listener::bind_long_path(&socket, |path| {
            let fd = listener::listen_private(&path)?;
            UnixListener::from_std(fd.into())
        })
        .context(format!("bind socket {}", socket.display()))?;
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
            // A client which goes away ends its connection with an error
            // as well, which is no failure of the server.
            let connection = rpc_system
                .inspect_err(|e| debug!("RPC connection ended: {e}"))
                .map(|_| ());
            task::spawn_local(Box::pin(connection));
        }
    }
}

#[cfg(test)]
mod tests;
