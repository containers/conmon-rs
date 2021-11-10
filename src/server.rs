use anyhow::{Context, Error, Result};
use async_stream::stream;
use clap::crate_version;
use conmon::{
    conmon_server::{Conmon, ConmonServer},
    VersionRequest, VersionResponse,
};
use env_logger::fmt::Color;
use futures::TryFutureExt;
use getset::{Getters, MutGetters};
use log::{debug, info, LevelFilter};
use std::{env, io::Write, path::PathBuf};
use stream::Stream;
use tokio::{
    fs,
    net::UnixListener,
    signal::unix::{signal, SignalKind},
    sync::oneshot,
};
use tonic::{transport::Server, Request, Response, Status};

mod config;
mod stream;

const VERSION: &str = crate_version!();

pub mod conmon {
    tonic::include_proto!("conmon");
}

#[derive(Debug, Default, Getters, MutGetters)]
pub struct ConmonServerImpl {
    #[doc = "The main conmon configuration."]
    #[getset(get, get_mut)]
    config: config::Config,
}

impl ConmonServerImpl {
    /// Create a new ConmonServerImpl instance.
    pub async fn new() -> Result<Self> {
        let server = Self::default();
        server.init_logging().context("set log verbosity")?;
        server
            .config()
            .validate()
            .await
            .context("validate config")?;
        Ok(server)
    }

    fn init_logging(&self) -> Result<()> {
        let level = self.config.log_level().to_string();
        env::set_var("RUST_LOG", level);

        // Initialize the logger with the format:
        // [YYYY-MM-DDTHH:MM:SS:MMMZ LEVEL crate::module file:LINE] MSG…
        // The file and line will be only printed when running with debug or trace level.
        let log_level = self.config.log_level();
        env_logger::builder()
            .format(move |buf, r| {
                let mut style = buf.style();
                style.set_color(Color::Black).set_intense(true);
                writeln!(
                    buf,
                    "{}{} {:<5} {}{}{} {}",
                    style.value("["),
                    buf.timestamp_millis(),
                    buf.default_styled_level(r.level()),
                    r.target(),
                    match (log_level >= LevelFilter::Debug, r.file(), r.line()) {
                        (true, Some(file), Some(line)) => format!(" {}:{}", file, line),
                        _ => "".into(),
                    },
                    style.value("]"),
                    r.args()
                )
            })
            .try_init()
            .context("init env logger")
    }
}

#[tonic::async_trait]
impl Conmon for ConmonServerImpl {
    async fn version(
        &self,
        request: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        info!("Got a request: {:?}", request);

        let res = VersionResponse {
            version: String::from(VERSION),
        };

        Ok(Response::new(res))
    }
}

// Use the single threaded runtime to save rss memory
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    let server = ConmonServerImpl::new().await?;

    let socket = server.config().socket().clone();
    let sigterm_handler = tokio::spawn(start_sigterm_handler(socket, shutdown_tx));
    let grpc_backend = tokio::spawn(start_grpc_backend(server, shutdown_rx));

    let _ = tokio::join!(sigterm_handler, grpc_backend);
    Ok(())
}

async fn start_sigterm_handler(socket: PathBuf, shutdown_tx: oneshot::Sender<()>) -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    sigterm.recv().await;
    info!("Received SIGTERM");
    let _ = shutdown_tx.send(());
    debug!("Removing socket file {}", socket.display());
    fs::remove_file(socket)
        .await
        .context("remove existing socket file")?;
    Ok(())
}

async fn start_grpc_backend(
    server: ConmonServerImpl,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<(), Error> {
    let incoming = {
        let uds = UnixListener::bind(server.config().socket()).context("bind server socket")?;
        stream! {
            loop {
                let item = uds.accept().map_ok(|(st, _)| Stream(st)).await;
                yield item;
            }
        }
    };

    Server::builder()
        .add_service(ConmonServer::new(server))
        .serve_with_incoming_shutdown(incoming, async move {
            let _ = shutdown_rx.await.ok();
            info!("gracefully shutting down grpc backend")
        })
        .await?;
    Ok(())
}
