use crate::{
    config::{Config, LogDriver},
    console::Console,
    init::{DefaultInit, Init},
};
use anyhow::{Context, Result};
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use conmon_common::conmon_capnp::conmon;
use futures::{AsyncReadExt, FutureExt};
use getset::{Getters, MutGetters};
use log::{debug, info};
use nix::{
    libc::_exit,
    sys::signal::Signal,
    unistd::{fork, ForkResult},
};
use std::{fs::File, io::Write, path::Path, process, sync::Arc};
use tokio::{
    fs,
    net::UnixListener,
    runtime,
    signal::unix::{signal, SignalKind},
    sync::oneshot,
    task::{self, LocalSet},
};
use tokio_util::compat::TokioAsyncReadCompatExt;
use twoparty::VatNetwork;

pub use version::Version;

mod child;
mod child_reaper;
mod config;
mod console;
mod cri_logger;
mod init;
mod iostreams;
mod rpc;
mod stream;
mod version;

#[derive(Debug, Default, Getters, MutGetters)]
pub struct Server {
    #[doc = "The main conmon configuration."]
    #[getset(get, get_mut)]
    config: Config,

    #[getset(get, get_mut)]
    reaper: Arc<child_reaper::ChildReaper>,
}

impl Server {
    /// Create a new Server instance.
    pub fn new() -> Result<Self> {
        let server = Self::default();

        if server.config().version() {
            Version::new().print();
            process::exit(0);
        }

        server.init_logging().context("set log verbosity")?;
        server.config().validate().context("validate config")?;

        server.init_self()?;
        Ok(server)
    }

    /// Start the conmon server instance by consuming it.
    pub fn start(self) -> Result<()> {
        // We need to fork as early as possible, especially before setting up tokio.
        // If we don't, the child will have a strange thread space and we're at risk of deadlocking.
        // We also have to treat the parent as the child (as described in [1]) to ensure we don't
        // interrupt the child's execution.
        // 1: https://docs.rs/nix/0.23.0/nix/unistd/fn.fork.html#safety
        if !self.config().skip_fork() {
            match unsafe { fork()? } {
                ForkResult::Parent { child, .. } => {
                    if let Some(path) = &self.config().conmon_pidfile() {
                        let child_str = format!("{}", child);
                        File::create(path)?.write_all(child_str.as_bytes())?;
                    }
                    unsafe { _exit(0) };
                }
                ForkResult::Child => (),
            }
        }

        // now that we've forked, set self to childreaper
        if let Err(errno) = prctl::set_child_subreaper(true) {
            return Err(anyhow::Error::new(std::io::Error::from_raw_os_error(errno)));
        }
        // Use the single threaded runtime to save rss memory.
        let rt = runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        rt.block_on(self.spawn_tasks())?;
        Ok(())
    }

    fn init_self(&self) -> Result<()> {
        let init = Init::<DefaultInit>::default();
        init.unset_locale()?;
        // While we could configure this, standard practice has it as -1000,
        // so it may be YAGNI to add configuration.
        init.set_oom_score("-1000")?;
        Ok(())
    }

    fn init_logging(&self) -> Result<()> {
        match self.config().log_driver() {
            LogDriver::Stdout => {
                simple_logger::init().context("init stdout logger")?;
                info!("Using stdout logger");
            }
            LogDriver::Systemd => {
                systemd_journal_logger::init().context("init journal logger")?;
                info!("Using systemd logger");
            }
        }
        log::set_max_level(self.config().log_level());
        info!("Set log level to: {}", self.config().log_level());
        Ok(())
    }

    /// Spwans all required tokio tasks.
    async fn spawn_tasks(self) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let socket = self.config().socket().to_path_buf();
        tokio::spawn(Self::start_signal_handler(
            Arc::clone(self.reaper()),
            socket,
            shutdown_tx,
        ));

        task::spawn_blocking(move || {
            let rt = runtime::Handle::current();
            rt.block_on(async {
                LocalSet::new()
                    .run_until(self.start_backend(shutdown_rx))
                    .await
            })
        })
        .await?
    }

    async fn start_signal_handler<T: AsRef<Path>>(
        reaper: Arc<child_reaper::ChildReaper>,
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

        let _ = shutdown_tx.send(());

        // TODO FIXME Ideally we would drop after socket file is removed,
        // but the removal is taking longer than 10 seconds, indicating someone
        // is keeping it open...
        reaper.kill_grandchildren(handled_sig)?;

        debug!("Removing socket file {}", socket.as_ref().display());
        fs::remove_file(socket)
            .await
            .context("remove existing socket file")?;

        Ok(())
    }

    async fn start_backend(self, mut shutdown_rx: oneshot::Receiver<()>) -> Result<()> {
        let listener = UnixListener::bind(&self.config().socket()).context("bind server socket")?;
        let client: conmon::Client = capnp_rpc::new_client(self);

        loop {
            let stream = tokio::select! {
                _ = &mut shutdown_rx => {
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

    /// Generate the OCI runtime CLI arguments from the provided parameters.
    fn generate_runtime_args(
        &self,
        params: &conmon::CreateContainerParams,
        maybe_console: &Option<Console>,
        pidfile: &Path,
    ) -> Result<Vec<String>> {
        let req = params.get()?.get_request()?;
        let id = req.get_id()?.to_string();
        let bundle_path = req.get_bundle_path()?.to_string();
        let mut args = vec![];
        let runtime_root = self.config().runtime_root();
        if let Some(rr) = runtime_root {
            args.push(format!("--root={}", rr.display()));
        }

        args.extend([
            "create".to_string(),
            "--bundle".to_string(),
            bundle_path,
            "--pid-file".to_string(),
            pidfile.display().to_string(),
        ]);

        if let Some(console) = maybe_console {
            args.push(format!("--console-socket={}", console.path().display()));
        }
        args.push(id);
        debug!("Runtime args {:?}", args.join(" "));
        Ok(args)
    }

    /// Generate the OCI runtime CLI arguments from the provided parameters.
    fn generate_exec_sync_args(
        &self,
        pidfile: &Path,
        console: Option<&Console>,
        params: &conmon::ExecSyncContainerParams,
    ) -> Result<Vec<String>> {
        let req = params.get()?.get_request()?;
        let id = req.get_id()?.to_string();
        let command = req.get_command()?;
        let runtime_root = self.config().runtime_root();

        let mut args = vec![];
        if let Some(rr) = runtime_root {
            args.push(format!("--root={}", rr.display()));
        }
        args.push("exec".to_string());
        args.push("-d".to_string());
        if let Some(console) = console {
            args.push(format!("--console-socket={}", console.path().display()));
            args.push("--tty".to_string());
        }
        args.push(format!("--pid-file={}", pidfile.display()));
        args.push(id);
        for value in command.iter() {
            args.push(value?.to_string());
        }

        debug!("Exec args {:?}", args.join(" "));
        Ok(args)
    }
}
