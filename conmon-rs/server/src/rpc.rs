use crate::Server;
use capnp::capability::Promise;
use capnp::Error;
use capnp_rpc::pry;
use clap::crate_version;
use conmon_common::conmon_capnp::conmon;
use log::debug;
use tokio::process::Command;

const VERSION: &str = crate_version!();

impl conmon::Server for Server {
    fn version(
        &mut self,
        _: conmon::VersionParams,
        mut results: conmon::VersionResults,
    ) -> Promise<(), capnp::Error> {
        debug!("Got a request");
        results.get().init_response().set_version(VERSION);
        Promise::ok(())
    }

    fn create_container(
        &mut self,
        params: conmon::CreateContainerParams,
        mut results: conmon::CreateContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        debug!(
            "Got a create container request for id {}",
            pry!(req.get_id())
        );
        let args = pry!(self.generate_runtime_args(&params));

        let mut child = pry!(Command::new(self.config().runtime()).args(args).spawn());

        let pid = pry!(child
            .id()
            .ok_or_else(|| Error::failed("Child of container creation had none id".into())));

        let id = pry!(req.get_id()).to_string();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => debug!("status for container ID {} is {}", id, status),
                Err(e) => debug!("failed to spawn runtime process {}", e),
            }
        });
        results.get().init_response().set_container_pid(pid);
        Promise::ok(())
    }
}
