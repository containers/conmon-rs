use tonic::{transport::Server, Request, Response, Status};

use conmon::conmon_server::{Conmon, ConmonServer};
use conmon::{VersionRequest, VersionResponse};

const VERSION: &str = "0.1.0";

pub mod conmon {
    tonic::include_proto!("conmon");
}

#[derive(Debug, Default)]
pub struct ConmonServerImpl {}

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
