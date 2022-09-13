use crate::{
    child::Child,
    container_io::{ContainerIO, SharedContainerIO},
    container_log::ContainerLog,
    server::Server,
    version::Version,
};
use anyhow::format_err;
use capnp::{capability::Promise, Error};
use capnp_rpc::pry;
use conmon_common::conmon_capnp::conmon;
use std::{
    path::{Path, PathBuf},
    process, str,
    time::Duration,
};
use tokio::time::Instant;
use tracing::{debug, debug_span, error, Instrument};
use uuid::Uuid;

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

macro_rules! new_root_span {
    ($name:expr, $container_id:expr) => {
        debug_span!(
            $name,
            container_id = $container_id,
            uuid = Uuid::new_v4().to_string().as_str()
        )
    };
}

macro_rules! capnp_vec_str {
    ($x:expr) => {
        capnp_vec!($x, String::from)
    };
}

macro_rules! capnp_vec_path {
    ($x:expr) => {
        capnp_vec!($x, PathBuf::from)
    };
}

macro_rules! capnp_vec {
    ($x:expr, $from:expr) => {
        pry!(pry!($x).iter().map(|r| r.map($from)).collect())
    };
}

impl conmon::Server for Server {
    /// Retrieve version information from the server.
    fn version(
        &mut self,
        params: conmon::VersionParams,
        mut results: conmon::VersionResults,
    ) -> Promise<(), capnp::Error> {
        debug!("Got a version request");

        let req = pry!(pry!(params.get()).get_request());
        let version = Version::new(req.get_verbose());

        let mut response = results.get().init_response();
        response.set_process_id(process::id());
        response.set_version(version.version());
        response.set_tag(version.tag());
        response.set_commit(version.commit());
        response.set_build_date(version.build_date());
        response.set_target(version.target());
        response.set_rust_version(version.rust_version());
        response.set_cargo_version(version.cargo_version());
        response.set_cargo_tree(version.cargo_tree());

        Promise::ok(())
    }

    /// Create a new container for the provided parameters.
    fn create_container(
        &mut self,
        params: conmon::CreateContainerParams,
        mut results: conmon::CreateContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry!(req.get_id()).to_string();
        let cleanup_cmd: Vec<String> = pry!(pry!(req.get_cleanup_cmd())
            .iter()
            .map(|s| s.map(String::from))
            .collect());

        let span = new_root_span!("create_container", id.as_str());
        let _enter = span.enter();

        debug!("Got a create container request");

        let log_drivers = pry!(req.get_log_drivers());
        let container_log = pry_err!(ContainerLog::from(log_drivers));
        let mut container_io =
            pry_err!(ContainerIO::new(req.get_terminal(), container_log.clone()));

        let bundle_path = Path::new(pry!(req.get_bundle_path()));
        let pidfile = bundle_path.join("pidfile");
        debug!("PID file is {}", pidfile.display());

        let child_reaper = self.reaper().clone();
        let global_args = capnp_vec_str!(req.get_global_args());
        let command_args = capnp_vec_str!(req.get_command_args());
        let args = pry_err!(self.generate_create_args(
            &id,
            bundle_path,
            &container_io,
            &pidfile,
            global_args,
            command_args
        ));
        let runtime = self.config().runtime().clone();
        let exit_paths = capnp_vec_path!(req.get_exit_paths());
        let oom_exit_paths = capnp_vec_path!(req.get_oom_exit_paths());

        Promise::from_future(
            async move {
                capnp_err!(container_log.write().await.init().await)?;

                let (grandchild_pid, token) = capnp_err!(match child_reaper
                    .create_child(runtime, args, &mut container_io, &pidfile)
                    .await
                {
                    Err(e) => {
                        // Attach the stderr output to the error message
                        let (_, stderr, _) =
                            capnp_err!(container_io.read_all_with_timeout(None).await)?;
                        if !stderr.is_empty() {
                            let stderr_str = str::from_utf8(&stderr)?;
                            Err(format_err!("{:#}: {}", e, stderr_str))
                        } else {
                            Err(e)
                        }
                    }
                    res => res,
                })?;

                // register grandchild with server
                let io = SharedContainerIO::new(container_io);
                let child = Child::new(
                    id,
                    grandchild_pid,
                    exit_paths,
                    oom_exit_paths,
                    None,
                    io,
                    cleanup_cmd,
                    token,
                );
                capnp_err!(child_reaper.watch_grandchild(child))?;

                results
                    .get()
                    .init_response()
                    .set_container_pid(grandchild_pid);
                Ok(())
            }
            .instrument(debug_span!("promise")),
        )
    }

    /// Execute a command in sync inside of a container.
    fn exec_sync_container(
        &mut self,
        params: conmon::ExecSyncContainerParams,
        mut results: conmon::ExecSyncContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry!(req.get_id()).to_string();
        let timeout = req.get_timeout_sec();

        let pidfile = pry_err!(ContainerIO::temp_file_name(
            Some(self.config().runtime_dir()),
            "exec_sync",
            "pid"
        ));

        let span = new_root_span!("exec_sync_container", id.as_str());
        let _enter = span.enter();

        debug!("Got exec sync container request with timeout {}", timeout);

        let runtime = self.config().runtime().clone();
        let child_reaper = self.reaper().clone();

        let logger = ContainerLog::new();
        let mut container_io = pry_err!(ContainerIO::new(req.get_terminal(), logger));

        let command = pry!(req.get_command());
        let args = pry_err!(self.generate_exec_sync_args(&id, &pidfile, &container_io, &command));

        Promise::from_future(
            async move {
                match child_reaper
                    .create_child(&runtime, &args, &mut container_io, &pidfile)
                    .await
                {
                    Ok((grandchild_pid, token)) => {
                        let time_to_timeout = if timeout > 0 {
                            Some(Instant::now() + Duration::from_secs(timeout))
                        } else {
                            None
                        };
                        let mut resp = results.get().init_response();
                        // register grandchild with server
                        let io = SharedContainerIO::new(container_io);
                        let io_clone = io.clone();
                        let child = Child::new(
                            id,
                            grandchild_pid,
                            vec![],
                            vec![],
                            time_to_timeout,
                            io_clone,
                            vec![],
                            token.clone(),
                        );

                        let mut exit_rx = capnp_err!(child_reaper.watch_grandchild(child))?;

                        let (stdout, stderr, timed_out) =
                            capnp_err!(io.read_all_with_timeout(time_to_timeout).await)?;

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
            }
            .instrument(debug_span!("promise")),
        )
    }

    /// Attach to a running container.
    fn attach_container(
        &mut self,
        params: conmon::AttachContainerParams,
        _: conmon::AttachContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let container_id = pry_err!(req.get_id());

        let span = new_root_span!("attach_container", container_id);
        let _enter = span.enter();

        debug!("Got a attach container request",);

        let exec_session_id = pry_err!(req.get_exec_session_id());
        if !exec_session_id.is_empty() {
            debug!("Using exec session id {}", exec_session_id);
        }

        let socket_path = pry!(req.get_socket_path()).to_string();
        let child = pry_err!(self.reaper().get(container_id));

        Promise::from_future(
            async move {
                capnp_err!(
                    child
                        .io()
                        .attach()
                        .await
                        .add(&socket_path, child.token().clone())
                        .await
                )
            }
            .instrument(debug_span!("promise")),
        )
    }

    /// Rotate all log drivers for a running container.
    fn reopen_log_container(
        &mut self,
        params: conmon::ReopenLogContainerParams,
        _: conmon::ReopenLogContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let container_id = pry_err!(req.get_id());

        let span = new_root_span!("reopen_log_container", container_id);
        let _enter = span.enter();

        debug!("Got a reopen container log request");

        let child = pry_err!(self.reaper().get(container_id));

        Promise::from_future(
            async move { capnp_err!(child.io().logger().await.write().await.reopen().await) }
                .instrument(debug_span!("promise")),
        )
    }

    /// Adjust the window size of a container running inside of a terminal.
    fn set_window_size_container(
        &mut self,
        params: conmon::SetWindowSizeContainerParams,
        _: conmon::SetWindowSizeContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let container_id = pry_err!(req.get_id());

        let span = new_root_span!("set_window_size_container", container_id);
        let _enter = span.enter();

        debug!("Got a set window size container request");

        let child = pry_err!(self.reaper().get(container_id));
        let width = req.get_width();
        let height = req.get_height();

        Promise::from_future(
            async move { capnp_err!(child.io().resize(width, height).await) }
                .instrument(debug_span!("promise")),
        )
    }
}
