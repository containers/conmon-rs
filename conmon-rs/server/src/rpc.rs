use crate::{
    attach::Attach,
    child::Child,
    child_reaper::ChildReaper,
    container_io::{ContainerIO, Message},
    server::Server,
    terminal::Terminal,
    version::Version,
};
use anyhow::Context;
use capnp::{capability::Promise, Error};
use capnp_rpc::pry;
use conmon_common::conmon_capnp::conmon;
use log::{debug, error};
use nix::sys::signal::Signal;
use std::{
    path::{Path, PathBuf},
    sync::{mpsc::RecvTimeoutError, Arc},
    time::Duration,
};

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
        let id = pry!(req.get_id()).to_string();
        debug!("Got a create container request for id {}", id);

        let log_path = PathBuf::from(pry!(req.get_log_path()));
        let terminal = req.get_terminal();
        let bundle_path = PathBuf::from(pry!(req.get_bundle_path()));
        let pidfile = bundle_path.join("pidfile");
        debug!("PID file is {}", pidfile.display());

        let child_reaper = Arc::clone(self.reaper());
        let runtime = self.config().runtime().clone();
        let runtime_root = self.config().runtime_root().clone();
        let id = pry_err!(req.get_id()).to_string();
        let exit_paths = pry!(pry!(req.get_exit_paths())
            .iter()
            .map(|r| r.map(PathBuf::from))
            .collect());

        Promise::from_future(async move {
            let container_io = capnp_err!(ContainerIO::new(terminal, &log_path)
                .await
                .context("create container IO"))?;

            let args = capnp_err!(Self::generate_runtime_args(
                runtime_root,
                &params,
                &container_io,
                &pidfile
            ))?;

            let grandchild_pid = capnp_err!(
                child_reaper
                    .create_child(runtime, &args, &container_io, pidfile)
                    .await
            )?;

            // register grandchild with server
            let child = Child::new(id, grandchild_pid, exit_paths, log_path);
            let stop_tx = container_io.stop_tx();
            capnp_err!(child_reaper.watch_grandchild(child, stop_tx))?;

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
        let pidfile = pry_err!(Terminal::temp_file_name("exec_sync", "pid"));

        debug!(
            "Got exec sync container request for id {} with timeout {}",
            id, timeout,
        );

        let terminal = req.get_terminal();
        let runtime = self.config().runtime().clone();
        let runtime_root = self.config().runtime_root().clone();

        let child_reaper = Arc::clone(self.reaper());

        // Verify the original container is still running
        // TODO: Do we need to do this while caller does?
        let reapable_child = if let Ok(c) = child_reaper.get(&id) {
            c
        } else {
            results.get().init_response().set_exit_code(-1);
            return Promise::ok(());
        };

        Promise::from_future(async move {
            // TODO: where to get the log path
            let log_path = reapable_child.log_path().clone();
            let container_io = capnp_err!(ContainerIO::new(terminal, &log_path)
                .await
                .context("create container IO"))?;

            let args = capnp_err!(Self::generate_exec_sync_args(
                runtime_root,
                &pidfile,
                &container_io,
                &params
            ))?;

            match child_reaper
                .create_child(&runtime, &args, &container_io, pidfile)
                .await
            {
                Ok(grandchild_pid) => {
                    let mut resp = results.get().init_response();
                    // register grandchild with server
                    let child = Child::new(id, grandchild_pid, vec![], log_path);

                    let stop_tx = container_io.stop_tx();
                    let mut exit_tx = capnp_err!(child_reaper.watch_grandchild(child, stop_tx))?;

                    let mut stdio = vec![];
                    let mut timed_out = false;
                    loop {
                        let msg = if timeout > 0 {
                            match container_io
                                .receiver()
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
                            capnp_err!(container_io.receiver().recv())?
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
                    results.get().init_response().set_exit_code(-2);
                }
            }
            Ok(())
        })
    }

    fn attach_container(
        &mut self,
        params: conmon::AttachContainerParams,
        _: conmon::AttachContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let container_id = pry_err!(req.get_id());
        debug!(
            "Got a attach container request for container id {}",
            container_id
        );

        let exec_session_id = pry_err!(req.get_exec_session_id());
        if !exec_session_id.is_empty() {
            debug!("Using exec session id {}", exec_session_id);
        }

        let socket_path = Path::new(pry!(req.get_socket_path()));
        let attach = pry_err!(Attach::new(socket_path).context("create attach endpoint"));

        let mut child = pry_err!(self.reaper().get(container_id));
        child.set_attach(attach.into());

        Promise::ok(())
    }
}
