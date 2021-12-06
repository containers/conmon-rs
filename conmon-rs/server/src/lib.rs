use crate::{
    console::Console,
    init::{DefaultInit, Init},
};
use anyhow::{Context, Result};
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use conmon_common::conmon_capnp::conmon;
use futures::{AsyncReadExt, FutureExt};
use getset::{Getters, MutGetters};
use log::{debug, info};
use log4rs::{
    append::file::FileAppender,
    config::{Appender, Config, Root},
    encode::pattern::PatternEncoder,
};
use nix::{
    libc::_exit,
    unistd::{fork, ForkResult},
};
use std::{fs::File, io::Write, path::Path};
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

mod config;
mod console;
mod cri_logger;
mod init;
mod rpc;

#[derive(Debug, Default, Getters, MutGetters)]
pub struct Server {
    #[doc = "The main conmon configuration."]
    #[getset(get, get_mut)]
    config: config::Config,
}

impl Server {
    /// Create a new Server instance.
    pub fn new() -> Result<Self> {
        let server = Self::default();
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

        // Use the single threaded runtime to save rss memory.
        let rt = runtime::Builder::new_current_thread().enable_io().build()?;
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
        match (self.config().log_file(), self.config().log_level()) {
            (Some(file), level) => {
                let logfile = FileAppender::builder()
                    .encoder(Box::new(PatternEncoder::new("{d} {l} [{M}:{L}] {m}\n")))
                    .build(file)?;

                let config = Config::builder()
                    .appender(Appender::builder().build("logfile", Box::new(logfile)))
                    .build(Root::builder().appender("logfile").build(level))?;

                log4rs::init_config(config).context("init logger")?;
                println!("Logging to file {}", file.display());
            }
            _ => println!("Logging disabled"),
        }

        Ok(())
    }

    /// Spwans all required tokio tasks.
    async fn spawn_tasks(self) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let socket = self.config().socket().to_path_buf();
        tokio::spawn(Self::start_sigterm_handler(socket, shutdown_tx));

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

    async fn start_sigterm_handler<T: AsRef<Path>>(
        socket: T,
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
        };

        let _ = shutdown_tx.send(());
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
    fn generate_runtime_args<T: AsRef<Path>>(
        &self,
        params: &conmon::CreateContainerParams,
        maybe_console: &Option<Console>,
        pidfile: T,
    ) -> Result<Vec<String>> {
        let req = params.get()?.get_request()?;
        let id = req.get_id()?.to_string();
        let bundle_path = req.get_bundle_path()?.to_string();
        let mut args = vec![];
        let runtime_root = self.config().runtime_root();
        if let Some(rr) = runtime_root {
            args.push(format!("--root={}", rr.display()));
        }

        args.extend(vec![
            "create".to_string(),
            "--bundle".to_string(),
            bundle_path,
            "--pid-file".to_string(),
            pidfile.as_ref().display().to_string(),
        ]);

        if let Some(console) = maybe_console {
            args.push(format!("--console-socket={}", console.path().display()));
        }
        args.push(id);
        debug!("Runtime args {:?}", args.join(" "));
        Ok(args)
    }
}
