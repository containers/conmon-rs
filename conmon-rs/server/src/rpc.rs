use crate::{
    child::Child,
    child_reaper::ChildReaper,
    console::{Console, Message},
    iostreams::IOStreams,
    version::Version,
    Server,
};
use capnp::{capability::Promise, Error};
use capnp_rpc::pry;
use conmon_common::conmon_capnp::conmon;
use log::{debug, error};
use nix::sys::signal::Signal;
use std::sync::mpsc::RecvTimeoutError;
use std::{path::PathBuf, sync::Arc, time::Duration};

macro_rules! pry_err {
    ($x:expr) => {
        pry!(capnp_err!($x))
    };
}

macro_rules! capnp_err {
    ($x:expr) => {
        $x.map_err(|e| Error::failed(format!("{:#}", e)))
    };
}

impl conmon::Server for Server {
    fn version(
        &mut self,
        _: conmon::VersionParams,
        mut results: conmon::VersionResults,
    ) -> Promise<(), capnp::Error> {
        debug!("Got a version request");
        let mut response = results.get().init_response();
        let version = Version::new();
        response.set_version(version.version());
        response.set_tag(version.tag());
        response.set_commit(version.commit());
        response.set_build_date(version.build_date());
        response.set_rust_version(version.rust_version());
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
            pry_err!(pry_err!(IOStreams::new()).start());
            None
        };

        let pidfile = pry!(pidfile_from_params(&params));
        let child_reaper = Arc::clone(self.reaper());
        let args = pry_err!(self.generate_runtime_args(&params, &maybe_console, &pidfile));
        let runtime = self.config().runtime().clone();
        let id = pry_err!(req.get_id()).to_string();
        let exit_paths = pry!(path_vec_from_text_list(pry!(req.get_exit_paths())));

        Promise::from_future(async move {
            let grandchild_pid = capnp_err!(
                child_reaper
                    .create_child(runtime, args, maybe_console.as_ref(), pidfile)
                    .await
            )?;

            // register grandchild with server
            let child = Child::new(id, grandchild_pid, exit_paths);
            capnp_err!(child_reaper.watch_grandchild(child))?;

            results
                .get()
                .init_response()
                .set_container_pid(grandchild_pid);
            Ok(())
        })
    }

    fn exec_sync_container(
        &mut self,
        params: conmon::ExecSyncContainerParams,
        mut results: conmon::ExecSyncContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry!(req.get_id()).to_string();
        let timeout = req.get_timeout_sec();
        let pidfile = pry_err!(Console::temp_file_name("exec_sync", "pid"));

        debug!(
            "Got exec sync container request for id {} with timeout {}",
            id, timeout,
        );

        let console = pry_err!(Console::new());
        let args = pry_err!(self.generate_exec_sync_args(&pidfile, Some(&console), &params));
        let runtime = self.config.runtime().clone();

        let child_reaper = Arc::clone(self.reaper());
        let child = if let Ok(c) = child_reaper.get(id.clone()) {
            c
        } else {
            let mut resp = results.get().init_response();
            resp.set_exit_code(-1);
            return Promise::ok(());
        };

        Promise::from_future(async move {
            match child_reaper
                .create_child(&runtime, &args, Some(&console), pidfile)
                .await
            {
                Ok(grandchild_pid) => {
                    let mut resp = results.get().init_response();
                    // register grandchild with server
                    let child = Child::new(id, grandchild_pid, child.exit_paths);

                    let mut exit_tx = capnp_err!(child_reaper.watch_grandchild(child))?;

                    let mut stdio = vec![];
                    let mut timed_out = false;
                    loop {
                        let msg = if timeout > 0 {
                            match console
                                .message_rx()
                                .recv_timeout(Duration::from_secs(timeout))
                            {
                                Ok(msg) => msg,
                                Err(e) => {
                                    if let RecvTimeoutError::Timeout = e {
                                        timed_out = true;
                                        capnp_err!(ChildReaper::kill_grandchild(
                                            grandchild_pid,
                                            Signal::SIGKILL
                                        ))?;
                                    }
                                    Message::Done
                                }
                            }
                        } else {
                            capnp_err!(console.message_rx().recv())?
                        };

                        match msg {
                            Message::Data(s) => stdio.extend(s),
                            Message::Done => break,
                        }
                    }

                    resp.set_timed_out(timed_out);
                    if !timed_out {
                        resp.set_stdout(&stdio);
                        resp.set_exit_code(capnp_err!(exit_tx.recv().await)?);
                    }
                }
                Err(e) => {
                    error!("Unable to create child: {:#}", e);
                    let mut resp = results.get().init_response();
                    resp.set_exit_code(-2);
                }
            }
            Ok(())
        })
    }
}

fn pidfile_from_params(params: &conmon::CreateContainerParams) -> capnp::Result<PathBuf> {
    let mut pidfile_pathbuf = PathBuf::from(params.get()?.get_request()?.get_bundle_path()?);
    pidfile_pathbuf.push("pidfile");

    debug!("PID file is {}", pidfile_pathbuf.display());
    Ok(pidfile_pathbuf)
}

fn path_vec_from_text_list(tl: capnp::text_list::Reader) -> Result<Vec<PathBuf>, capnp::Error> {
    tl.iter().map(|r| r.map(PathBuf::from)).collect()
}
