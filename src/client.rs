use anyhow::Context;
use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use conmon_capnp::conmon;
use futures::{AsyncReadExt, FutureExt};
use std::net::ToSocketAddrs;

pub mod conmon_capnp {
    include!(concat!(env!("OUT_DIR"), "/proto/conmon_capnp.rs"));
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50051"
        .to_socket_addrs()?
        .next()
        .context("could not parse address")?;

    tokio::task::LocalSet::new()
        .run_until(async move {
            let stream = tokio::net::TcpStream::connect(&addr).await?;
            stream.set_nodelay(true)?;
            let (reader, writer) =
                tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();
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
