use crate::Server;
use capnp::capability::Promise;
use capnp_rpc::pry;
use clap::crate_version;
use conmon_common::conmon_capnp::conmon;
use log::info;

const VERSION: &str = crate_version!();

impl conmon::Server for Server {
    fn version(
        &mut self,
        _: conmon::VersionParams,
        mut results: conmon::VersionResults,
    ) -> Promise<(), capnp::Error> {
        info!("Got a request");
        results.get().init_response().set_version(VERSION);
        Promise::ok(())
    }

    fn create_container(
        &mut self,
        params: conmon::CreateContainerParams,
        mut results: conmon::CreateContainerResults,
    ) -> Promise<(), capnp::Error> {
        info!("Got a create container request for id {}", pry!(pry!(pry!(params.get()).get_request()).get_id()));
        results.get().init_response().set_container_pid(0);
        Promise::ok(())
    }
}
