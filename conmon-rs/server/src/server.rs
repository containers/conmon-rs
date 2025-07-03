#![deny(missing_docs)]

use crate::{
    child_reaper::ChildReaper,
    config::{Commands, Config, LogDriver, LogLevel, Verbosity},
    container_io::{ContainerIO, ContainerIOType},
    fd_socket::FdSocket,
    init::{DefaultInit, Init},
    journal::Journal,
    listener::{DefaultListener, Listener},
    pause::Pause,
    streaming_server::StreamingServer,
    telemetry::Telemetry,
    version::Version,
};
use anyhow::{Context, Result, format_err};
use capnp::text_list::Reader;
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp::Side, twoparty};
use clap::crate_name;
use conmon_common::conmon_capnp::conmon::{self, CgroupManager};
use futures::{AsyncReadExt, FutureExt};
use getset::Getters;
use nix::{
    errno::Errno,
    libc::_exit,
    sys::signal::Signal,
    unistd::{ForkResult, fork},
};
use opentelemetry::trace::{FutureExt as OpenTelemetryFutureExt, TracerProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::{fs::File, io::Write, path::Path, process, str::FromStr, sync::Arc};
use tokio::{
    fs,
    runtime::{Builder, Handle},
    signal::unix::{SignalKind, signal},
    sync::{RwLock, oneshot},
    task::{self, LocalSet},
};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{Instrument, debug, debug_span, info};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, prelude::*};
use twoparty::VatNetwork;

#[derive(Debug, Getters)]
/// The main server structure.
pub struct Server {
    /// Server configuration.
    #[getset(get = "pub(crate)")]
    config: Config,

    /// Child reaper instance.
    #[getset(get = "pub(crate)")]
    reaper: Arc<ChildReaper>,

    /// Fd socket instance.
    #[getset(get = "pub(crate)")]
    fd_socket: Arc<FdSocket>,

    /// OpenTelemetry tracer instance.
    #[getset(get = "pub(crate)")]
    tracer: Option<SdkTracerProvider>,

    /// Streaming server instance.
    #[getset(get = "pub(crate)")]
    streaming_server: Arc<RwLock<StreamingServer>>,
}

impl Server {
    /// Create a new `Server` instance.
    pub fn new() -> Result<Self> {
        let server = Self {
            config: Default::default(),
            reaper: Default::default(),
            fd_socket: Default::default(),
            tracer: Default::default(),
            streaming_server: Default::default(),
        };

        if let Some(v) = server.config().version() {
            Version::new(v == Verbosity::Full).print();
            process::exit(0);
        }

        if let Some(v) = server.config().version_json() {
            Version::new(v == Verbosity::Full).print_json()?;
            process::exit(0);
        }

        if let Some(Commands::Pause {
            base_path,
            pod_id,
            ipc,
            pid,
            net,
            user,
            uts,
            uid_mappings,
            gid_mappings,
        }) = server.config().command()
        {
            Pause::run(
                base_path,
                pod_id,
                *ipc,
                *pid,
                *net,
                *user,
                *uts,
                uid_mappings,
                gid_mappings,
            )
            .context("run pause")?;
            process::exit(0);
        }

        server.config().validate().context("validate config")?;

        Self::init().context("init self")?;
        Ok(server)
    }

    /// Start the `Server` instance and consume it.
    pub fn start(self) -> Result<()> {
        // We need to fork as early as possible, especially before setting up tokio.
        // If we don't, the child will have a strange thread space and we're at risk of deadlocking.
        // We also have to treat the parent as the child (as described in [1]) to ensure we don't
        // interrupt the child's execution.
        // 1: https://docs.rs/nix/0.23.0/nix/unistd/fn.fork.html#safety
        if !self.config().skip_fork() {
            match unsafe { fork()? } {
                ForkResult::Parent { child, .. } => {
                    let child_str = format!("{child}");
                    File::create(self.config().conmon_pidfile())?
                        .write_all(child_str.as_bytes())?;
                    unsafe { _exit(0) };
                }
                ForkResult::Child => (),
            }
        }

        // now that we've forked, set self to childreaper
        prctl::set_child_subreaper(true)
            .map_err(Errno::from_raw)
            .context("set child subreaper")?;

        let tracer = self.tracer().clone();

        let rt = Builder::new_multi_thread().enable_all().build()?;
        rt.block_on(self.spawn_tasks())?;

        if let Some(tracer) = tracer {
            tracer.shutdown().context("shutdown tracer")?;
        }

        rt.shutdown_background();
        Ok(())
    }

    fn init() -> Result<()> {
        let init = Init::<DefaultInit>::default();
        init.unset_locale()?;
        init.set_default_umask();
        // While we could configure this, standard practice has it as -1000,
        // so it may be YAGNI to add configuration.
        init.set_oom_score("-1000")
    }

    fn init_logging(&mut self) -> Result<()> {
        let level = LevelFilter::from_str(self.config().log_level().as_ref())
            .context("convert log level filter")?;

        let telemetry_layer = if self.config().enable_tracing() {
            let tracer = Telemetry::layer(self.config().tracing_endpoint())
                .context("build telemetry layer")?;

            self.tracer = Some(tracer.clone());

            tracing_opentelemetry::layer()
                .with_tracer(tracer.tracer(crate_name!()))
                .into()
        } else {
            None
        };

        let registry = tracing_subscriber::registry().with(telemetry_layer);

        match self.config().log_driver() {
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
                    .with_writer(Journal)
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

    /// Spawns all required tokio tasks.
    async fn spawn_tasks(mut self) -> Result<()> {
        self.init_logging().context("init logging")?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let socket = self.config().socket();
        let fd_socket = self.config().fd_socket();
        let reaper = self.reaper.clone();

        let signal_handler_span = debug_span!("signal_handler");
        task::spawn(
            Self::start_signal_handler(reaper, socket, fd_socket, shutdown_tx)
                .with_context(signal_handler_span.context())
                .instrument(signal_handler_span),
        );

        let backend_span = debug_span!("backend");
        task::spawn_blocking(move || {
            Handle::current().block_on(
                LocalSet::new()
                    .run_until(self.start_backend(shutdown_rx))
                    .with_context(backend_span.context())
                    .instrument(backend_span),
            )
        })
        .await?
    }

    async fn start_signal_handler<T: AsRef<Path>>(
        reaper: Arc<ChildReaper>,
        socket: T,
        fd_socket: T,
        shutdown_tx: oneshot::Sender<()>,
    ) -> Result<()> {
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let handled_sig: Signal;

        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM");
                handled_sig = Signal::SIGTERM;
            }
            _ = sigint.recv() => {
                info!("Received SIGINT");
                handled_sig = Signal::SIGINT;
            }
        }

        if let Some(pause) = Pause::maybe_shared() {
            pause.stop();
        }

        debug!("Starting grandchildren cleanup task");
        reaper
            .kill_grandchildren(handled_sig)
            .context("unable to kill grandchildren")?;

        debug!("Sending shutdown message");
        shutdown_tx
            .send(())
            .map_err(|_| format_err!("unable to send shutdown message"))?;

        debug!("Removing socket file {}", socket.as_ref().display());
        fs::remove_file(socket)
            .await
            .context("remove existing socket file")?;

        debug!("Removing fd socket file {}", fd_socket.as_ref().display());
        fs::remove_file(fd_socket)
            .await
            .or_else(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(err)
                }
            })
            .context("remove existing fd socket file")
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
                stream = listener.accept() => {
                    stream?.0
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
        global_args: Reader,
        command_args: Reader,
    ) -> Result<Vec<String>> {
        let mut args = self.default_args().context("build default runtime args")?;

        if let Some(rr) = self.config.runtime_root() {
            args.push(format!("--root={}", rr.display()));
        }

        if self.cgroup_manager == CgroupManager::Systemd {
            args.push(Self::SYSTEMD_CGROUP_ARG.into());
        }

        for arg in global_args {
            args.push(arg?.to_string()?);
        }

        args.extend([
            "create".to_string(),
            "--bundle".to_string(),
            bundle_path.display().to_string(),
            "--pid-file".to_string(),
            self.pidfile.display().to_string(),
        ]);

        for arg in command_args {
            args.push(arg?.to_string()?);
        }

        if let ContainerIOType::Terminal(terminal) = self.container_io.typ() {
            args.push(format!("--console-socket={}", terminal.path().display()));
        }

        args.push(self.id.into());

        debug!("Runtime args {:?}", args.join(" "));
        Ok(args)
    }

    /// Generate the OCI runtime CLI arguments from the provided parameters.
    pub(crate) fn exec_sync_args(&self, command: Reader) -> Result<Vec<String>> {
        let mut args = self
            .exec_sync_args_without_command()
            .context("exec sync args without command")?;

        for arg in command {
            args.push(arg?.to_string()?);
        }

        debug!("Exec args {:?}", args.join(" "));
        Ok(args)
    }

    pub(crate) fn exec_sync_args_without_command(&self) -> Result<Vec<String>> {
        let mut args = self.default_args().context("build default runtime args")?;

        if let Some(rr) = self.config.runtime_root() {
            args.push(format!("--root={}", rr.display()));
        }

        if self.cgroup_manager == CgroupManager::Systemd {
            args.push(Self::SYSTEMD_CGROUP_ARG.into());
        }

        args.push("exec".to_string());
        args.push("-d".to_string());

        if let ContainerIOType::Terminal(terminal) = self.container_io.typ() {
            args.push(format!("--console-socket={}", terminal.path().display()));
            args.push("--tty".to_string());
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
