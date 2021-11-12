use anyhow::{Context, Error, Result};
use capnp::capability::Promise;
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use clap::crate_version;
use conmon_capnp::conmon;
use futures::{AsyncReadExt, FutureExt};
use getset::{Getters, MutGetters};
use log::{debug, info};
use nix::{
    libc::_exit,
    unistd::{fork, ForkResult},
};
use std::{env, fs::File, io::Write, path::PathBuf};
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
mod init;

const VERSION: &str = crate_version!();

pub mod conmon_capnp {
    include!(concat!(env!("OUT_DIR"), "/proto/conmon_capnp.rs"));
}

#[derive(Debug, Default, Getters, MutGetters)]
pub struct ConmonServerImpl {
    #[doc = "The main conmon configuration."]
    #[getset(get, get_mut)]
    config: config::Config,
}

impl ConmonServerImpl {
    /// Create a new ConmonServerImpl instance.
    pub fn new() -> Result<Self> {
        let server = Self::default();
        server.init_logging().context("set log verbosity")?;
        server.config().validate().context("validate config")?;

        server.init_self()?;
        Ok(server)
    }

    fn init_self(&self) -> Result<(), Error> {
        init::unset_locale();
        // While we could configure this, standard practice has it as -1000,
        // so it may be YAGNI to add configuration.
        init::set_oom("-1000")?;
        Ok(())
    }

    fn init_logging(&self) -> Result<()> {
        if let Some(level) = self.config().log_level().to_level() {
            simple_logger::init_with_level(level).context("init logger")?;
            info!("Set log level to {}", level);
        }
        Ok(())
    }
}

impl conmon::Server for ConmonServerImpl {
    fn version(
        &mut self,
        _: conmon::VersionParams,
        mut results: conmon::VersionResults,
    ) -> Promise<(), capnp::Error> {
        info!("Got a request");
        results.get().init_response().set_version(VERSION);
        Promise::ok(())
    }
}

fn main() -> Result<(), Error> {
    // First, initialize the server so we have access to the config pre-fork.
    let server = ConmonServerImpl::new()?;

    // We need to fork as early as possible, especially before setting up tokio.
    // If we don't, the child will have a strange thread space and we're at risk of deadlocking.
    // We also have to treat the parent as the child (as described in [1]) to ensure we don't
    // interrupt the child's execution.
    // 1: https://docs.rs/nix/0.23.0/nix/unistd/fn.fork.html#safety
    match unsafe { fork()? } {
        ForkResult::Parent { child, .. } => {
            if let Some(path) = server.config().conmon_pidfile() {
                let child_str = format!("{}", child);
                File::create(path)?.write_all(child_str.as_bytes())?;
            }
            unsafe { _exit(0) };
        }
        ForkResult::Child => (),
    }
    // Use the single threaded runtime to save rss memory.
    let rt = runtime::Builder::new_current_thread().enable_io().build()?;
    rt.block_on(start_server(server))?;
    Ok(())
}

async fn start_server(server: ConmonServerImpl) -> Result<(), Error> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let socket = server.config().socket().clone();
    tokio::spawn(start_sigterm_handler(socket, shutdown_tx));

    task::spawn_blocking(move || {
        let rt = runtime::Handle::current();
        rt.block_on(async {
            LocalSet::new()
                .run_until(start_backend(server, shutdown_rx))
                .await
        })
    })
    .await?
}

async fn start_sigterm_handler(socket: PathBuf, shutdown_tx: oneshot::Sender<()>) -> Result<()> {
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
    debug!("Removing socket file {}", socket.display());
    fs::remove_file(socket)
        .await
        .context("remove existing socket file")?;
    Ok(())
}

async fn start_backend(
    server: ConmonServerImpl,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<(), Error> {
    let listener = UnixListener::bind(server.config().socket()).context("bind server socket")?;
    let client: conmon::Client = capnp_rpc::new_client(server);

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
