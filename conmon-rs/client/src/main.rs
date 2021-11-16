use async_net::unix;
use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use conmon_rs_common::conmon_capnp::conmon;
use futures::{AsyncReadExt, FutureExt};
use std::os::unix::net::UnixStream;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let stream = UnixStream::connect("conmon.sock")?;
            let stream: unix::UnixStream = async_io::Async::new(stream)?.into();
            let (reader, writer) = stream.split();

            let rpc_network = Box::new(twoparty::VatNetwork::new(
                reader,
                writer,
                rpc_twoparty_capnp::Side::Client,
                Default::default(),
            ));

            let mut rpc_system = RpcSystem::new(rpc_network, None);
            let client: conmon::Client = rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);

            tokio::task::spawn_local(Box::pin(rpc_system.map(|_| ())));

            let request = client.version_request();
            let response = request.send().promise.await?;

            println!(
                "received: {}",
                response.get()?.get_response()?.get_version()?
            );
            Ok(())
        })
        .await
}
