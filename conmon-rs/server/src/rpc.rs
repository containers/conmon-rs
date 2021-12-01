use crate::{console::Console, Server};
use anyhow::Context;
use capnp::{capability::Promise, Error};
use capnp_rpc::pry;
use clap::crate_version;
use conmon_common::conmon_capnp::conmon;
use log::debug;
use std::fs;
use std::path::PathBuf;

const VERSION: &str = crate_version!();

macro_rules! pry_err {
    ($x:expr) => {
        pry!($x.map_err(|e| Error::failed(format!("{:#}", e))))
    };
}

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

        let maybe_console = if req.get_terminal() {
            pry_err!(Console::new()).into()
        } else {
            None
        };

        let pidfile = pry!(pidfile_from_params(&params));

        let args = pry_err!(self.generate_runtime_args(&params, &maybe_console, &pidfile));

        let mut child = pry!(std::process::Command::new(self.config().runtime())
            .args(args)
            .spawn());

        let id = pry!(req.get_id()).to_string();
        let status = pry!(child.wait());
        debug!("Status for container ID {} is {}", id, status);
        if let Some(console) = maybe_console {
            pry_err!(console
                .wait_connected()
                .context("wait for console socket connection"));
        }

        let pid = pry_err!(pry!(fs::read_to_string(pidfile)).parse::<u32>());

        results.get().init_response().set_container_pid(pid);
        Promise::ok(())
    }
}

fn pidfile_from_params(params: &conmon::CreateContainerParams) -> capnp::Result<PathBuf> {
    let mut pidfile_pathbuf = PathBuf::from(params.get()?.get_request()?.get_bundle_path()?);
    pidfile_pathbuf.push("pidfile");

    debug!("pidfile is {}", pidfile_pathbuf.display());
    Ok(pidfile_pathbuf)
}
