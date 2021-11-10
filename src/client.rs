use conmon::conmon_client::ConmonClient;
use conmon::VersionRequest;
use std::convert::TryFrom;
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

pub mod conmon {
    tonic::include_proto!("conmon");
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let channel = Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(service_fn(|_: Uri| UnixStream::connect("conmon.sock")))
        .await?;
    let mut client = ConmonClient::new(channel);

    let req = tonic::Request::new(VersionRequest {});

    let resp = client.version(req).await?;

    println!("Version: {:?}", resp);

    Ok(())
}
