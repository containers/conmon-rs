#![deny(missing_docs)]

use crate::{
    child_reaper::ChildReaper,
    config::{CgroupManager, Config, LogDriver, VERSION_FULL},
    container_io::{ContainerIO, ContainerIOType},
    init::{DefaultInit, Init},
    listener::{DefaultListener, Listener},
    version::Version,
};
use anyhow::{format_err, Context, Result};
use capnp::text_list::Reader;
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use conmon_common::conmon_capnp::conmon;
use futures::{AsyncReadExt, FutureExt};
use getset::Getters;
use nix::{
    errno,
    libc::_exit,
    sys::signal::Signal,
    unistd::{fork, ForkResult},
};
use std::{fs::File, io::Write, path::Path, process, str::FromStr, sync::Arc};
use tokio::{
    fs,
    runtime::{Builder, Handle},
    signal::unix::{signal, SignalKind},
    sync::oneshot,
    task::{self, LocalSet},
};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{debug, debug_span, info, Instrument};
use tracing_subscriber::{filter::LevelFilter, prelude::*};
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
}

impl Server {
    /// Create a new `Server` instance.
    pub fn new() -> Result<Self> {
        let server = Self {
            config: Default::default(),
            reaper: Default::default(),
        };

        if let Some(v) = server.config().version() {
            Version::new(v == VERSION_FULL).print();
            process::exit(0);
        }

        server.init_logging().context("set log verbosity")?;
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
                    let child_str = format!("{}", child);
                    File::create(self.config().conmon_pidfile())?
                        .write_all(child_str.as_bytes())?;
                    unsafe { _exit(0) };
                }
                ForkResult::Child => (),
            }
        }

        // now that we've forked, set self to childreaper
        prctl::set_child_subreaper(true)
            .map_err(errno::from_i32)
            .context("set child subreaper")?;

        let rt = Builder::new_multi_thread().enable_all().build()?;
        rt.block_on(self.spawn_tasks())?;
        rt.shutdown_background();
        Ok(())
    }

    fn init() -> Result<()> {
        let init = Init::<DefaultInit>::default();
        init.unset_locale()?;
        // While we could configure this, standard practice has it as -1000,
        // so it may be YAGNI to add configuration.
        init.set_oom_score("-1000")
    }

    fn init_logging(&self) -> Result<()> {
        let level =
            LevelFilter::from_str(self.config().log_level()).context("convert log level filter")?;
        let registry = tracing_subscriber::registry();

        let stdout = if self.config().log_drivers().contains(&LogDriver::Stdout) {
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_line_number(true)
                .with_filter(level)
                .into()
        } else {
            None
        };

        let systemd = if self.config().log_drivers().contains(&LogDriver::Systemd) {
            tracing_journald::layer()
                .context("unable to connect to journald")?
                .with_filter(level)
                .into()
        } else {
            None
        };

        registry
            .with(stdout)
            .with(systemd)
            .try_init()
            .context("init registry")?;
        info!("Set log level to: {}", self.config().log_level());

        Ok(())
    }

    /// Spwans all required tokio tasks.
    async fn spawn_tasks(self) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let socket = self.config().socket();
        let reaper = self.reaper.clone();
        task::spawn(
            Self::start_signal_handler(reaper, socket, shutdown_tx)
                .instrument(debug_span!("signal_handler")),
        );

        task::spawn_blocking(move || {
            Handle::current().block_on(
                async {
                    LocalSet::new()
                        .run_until(self.start_backend(shutdown_rx))
                        .await
                }
                .instrument(debug_span!("backend")),
            )
        })
        .await?
    }

    async fn start_signal_handler<T: AsRef<Path>>(
        reaper: Arc<ChildReaper>,
        socket: T,
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
        };

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
            .context("remove existing socket file")
    }

    async fn start_backend(self, mut shutdown_rx: oneshot::Receiver<()>) -> Result<()> {
        let listener =
            Listener::<DefaultListener>::default().bind_long_path(&self.config().socket())?;
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

    const SYSTEMD_CGROUP_ARG: &'static str = "--systemd-cgroup";

    /// Generate the OCI runtime CLI arguments from the provided parameters.
    pub(crate) fn generate_create_args(
        &self,
        id: &str,
        bundle_path: &Path,
        container_io: &ContainerIO,
        pidfile: &Path,
        global_args: Vec<String>,
        command_args: Vec<String>,
    ) -> Result<Vec<String>> {
        let mut args = vec![];

        if let Some(rr) = self.config().runtime_root() {
            args.push(format!("--root={}", rr.display()));
        }

        if self.config().cgroup_manager() == CgroupManager::Systemd {
            args.push(Self::SYSTEMD_CGROUP_ARG.into());
        }

        args.extend(global_args);

        args.extend([
            "create".to_string(),
            "--bundle".to_string(),
            bundle_path.display().to_string(),
            "--pid-file".to_string(),
            pidfile.display().to_string(),
        ]);

        args.extend(command_args);

        if let ContainerIOType::Terminal(terminal) = container_io.typ() {
            args.push(format!("--console-socket={}", terminal.path().display()));
        }
        args.push(id.into());
        debug!("Runtime args {:?}", args.join(" "));
        Ok(args)
    }

    /// Generate the OCI runtime CLI arguments from the provided parameters.
    pub(crate) fn generate_exec_sync_args(
        &self,
        id: &str,
        pidfile: &Path,
        container_io: &ContainerIO,
        command: &Reader,
    ) -> Result<Vec<String>> {
        let mut args = vec![];

        if let Some(rr) = self.config().runtime_root() {
            args.push(format!("--root={}", rr.display()));
        }

        if self.config().cgroup_manager() == CgroupManager::Systemd {
            args.push(Self::SYSTEMD_CGROUP_ARG.into());
        }

        args.push("exec".to_string());
        args.push("-d".to_string());

        if let ContainerIOType::Terminal(terminal) = container_io.typ() {
            args.push(format!("--console-socket={}", terminal.path().display()));
            args.push("--tty".to_string());
        }

        args.push(format!("--pid-file={}", pidfile.display()));
        args.push(id.into());

        for value in command.iter() {
            args.push(value?.to_string());
        }

        debug!("Exec args {:?}", args.join(" "));
        Ok(args)
    }
}
