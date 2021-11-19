use crate::Server;
use capnp::capability::Promise;
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
}
