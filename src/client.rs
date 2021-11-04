use conmon::conmon_client::ConmonClient;
use conmon::VersionRequest;

pub mod conmon {
    tonic::include_proto!("conmon");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = ConmonClient::connect("http://[::1]:50051").await?;

    let req = tonic::Request::new(VersionRequest{});

    let resp = client.version(req).await?;

    println!("Version: {:?}", resp);

    Ok(())
}
