use tonic::{transport::Server, Request, Response, Status};

mod config;

use conmon::conmon_server::{Conmon, ConmonServer};
use conmon::{VersionRequest, VersionResponse};
use derive_builder::Builder;
use getset::{Getters, MutGetters};

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

#[tonic::async_trait]
impl Conmon for ConmonServerImpl {
    async fn version(
        &self,
        request: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        println!("Got a request: {:?}", request);

        let res = VersionResponse {
            version: String::from(VERSION),
        };

        Ok(Response::new(res))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50051".parse()?;
    let server = ConmonServerImpl::default();

    Server::builder()
        .add_service(ConmonServer::new(server))
        .serve(addr)
        .await?;

    Ok(())
}
