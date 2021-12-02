use crate::{console::Console, Server};
use anyhow::{bail, Context};
use capnp::{capability::Promise, Error};
use capnp_rpc::pry;
use clap::crate_version;
use conmon_common::conmon_capnp::conmon;
use log::{debug, error};
use tokio::process::Command;

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

        let args = pry!(self.generate_runtime_args(&params, &maybe_console));

        let mut child = pry!(Command::new(self.config().runtime()).args(args).spawn());

        let pid = pry!(child
            .id()
            .ok_or_else(|| Error::failed("Child of container creation had none id".into())));

        let id = pry!(req.get_id()).to_string();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    debug!("Status for container ID {} is {}", id, status);
                    if let Some(console) = maybe_console {
                        console
                            .wait_connected()
                            .context("wait for console socket connection")?;
                    }
                    Ok(())
                }
                Err(e) => {
                    error!("Failed to spawn runtime process {}", e);
                    bail!(e)
                }
            }
        });
        results.get().init_response().set_container_pid(pid);
        Promise::ok(())
    }
}
