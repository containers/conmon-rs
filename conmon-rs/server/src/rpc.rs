use crate::{
    attach::Attach, child::Child, container_io::ContainerIO, container_log::ContainerLog,
    server::Server, terminal::Terminal, version::Version,
};
use anyhow::Context;
use capnp::{capability::Promise, Error};
use capnp_rpc::pry;
use conmon_common::conmon_capnp::conmon;
use log::{debug, error};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::Instant;

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
        response.set_process_id(std::process::id());
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

        let log_drivers = pry!(req.get_log_drivers());
        let container_log = pry_err!(ContainerLog::from(log_drivers));
        let container_io = pry_err!(ContainerIO::new(req.get_terminal(), container_log.clone()));
        let bundle_path = PathBuf::from(pry!(req.get_bundle_path()));
        let pidfile = bundle_path.join("pidfile");
        debug!("PID file is {}", pidfile.display());

        let child_reaper = self.reaper().clone();
        let args = pry_err!(self.generate_runtime_args(&params, &container_io, &pidfile));
        let runtime = self.config().runtime().clone();
        let exit_paths = pry!(pry!(req.get_exit_paths())
            .iter()
            .map(|r| r.map(PathBuf::from))
            .collect());

        Promise::from_future(async move {
            capnp_err!(container_log.write().await.init().await)?;

            let grandchild_pid = capnp_err!(
                child_reaper
                    .create_child(runtime, args, &container_io, pidfile)
                    .await
            )?;

            // register grandchild with server
            let child = Child::new(id, grandchild_pid, exit_paths, container_log, None);
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
        // TODO FIXME: add defer style removal--possibly with a macro or creating a special type
        // that can be dropped?
        let pidfile = pry_err!(Terminal::temp_file_name(
            Some(self.config().runtime_dir()),
            "exec_sync",
            "pid"
        ));

        debug!(
            "Got exec sync container request for id {} with timeout {}",
            id, timeout,
        );

        let runtime = self.config().runtime().clone();
        let child_reaper = self.reaper().clone();

        let logger = ContainerLog::new();
        let mut container_io = pry_err!(ContainerIO::new(req.get_terminal(), logger.clone()));
        let args = pry_err!(self.generate_exec_sync_args(&pidfile, &container_io, &params));

        Promise::from_future(async move {
            match child_reaper
                .create_child(&runtime, &args, &container_io, pidfile)
                .await
            {
                Ok(grandchild_pid) => {
                    let time_to_timeout = if timeout > 0 {
                        Some(Instant::now() + Duration::from_secs(timeout))
                    } else {
                        None
                    };
                    let mut resp = results.get().init_response();
                    // register grandchild with server
                    let child = Child::new(id, grandchild_pid, vec![], logger, time_to_timeout);

                    let mut exit_rx = capnp_err!(child_reaper.watch_grandchild(child))?;

                    let (stdout, stderr, timed_out) =
                        container_io.read_all_with_timeout(time_to_timeout).await;

                    let exit_data = capnp_err!(exit_rx.recv().await)?;
                    resp.set_stdout(&stdout);
                    resp.set_stderr(&stderr);
                    resp.set_exit_code(*exit_data.exit_code());
                    if timed_out || exit_data.timed_out {
                        resp.set_timed_out(true);
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

    fn reopen_log_container(
        &mut self,
        params: conmon::ReopenLogContainerParams,
        _: conmon::ReopenLogContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let container_id = pry_err!(req.get_id());
        debug!(
            "Got a reopen container log request for container id {}",
            container_id
        );

        let child = pry_err!(self.reaper().get(container_id));

        Promise::from_future(async move {
            capnp_err!(child.logger().write().await.reopen().await)?;
            Ok(())
        })
    }
}
