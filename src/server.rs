use anyhow::{Context, Result};
use conmon::{
    conmon_server::{Conmon, ConmonServer},
    VersionRequest, VersionResponse,
};
use derive_builder::Builder;
use env_logger::fmt::Color;
use getset::{Getters, MutGetters};
use log::{info, LevelFilter};
use std::{env, io::Write};
use tonic::{transport::Server, Request, Response, Status};

mod config;

const VERSION: &str = "0.1.0";

pub mod conmon {
    tonic::include_proto!("conmon");
}

#[derive(Builder, Debug, Default, Getters, MutGetters)]
#[builder(default, pattern = "owned", setter(into))]
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50051".parse()?;
    let server = ConmonServerImpl::new()?;

    Server::builder()
        .add_service(ConmonServer::new(server))
        .serve(addr)
        .await?;

    Ok(())
}
