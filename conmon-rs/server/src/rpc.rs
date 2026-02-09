use crate::{
    capnp_util,
    child::Child,
    container_io::{ContainerIO, SharedContainerIO},
    container_log::ContainerLog,
    pause::Pause,
    server::{GenerateRuntimeArgs, Server},
    telemetry::Telemetry,
    version::Version,
};
use anyhow::{Context, format_err};
use capnp::{Error, capability::Promise};
use capnp_rpc::pry;
use conmon_common::conmon_capnp::conmon;
use std::{
    path::{Path, PathBuf},
    process,
    rc::Rc,
    str,
    time::Duration,
};
use tokio::time::Instant;
use tracing::{Instrument, debug, debug_span, error};
use uuid::Uuid;

macro_rules! pry_err {
    ($x:expr_2021) => {
        pry!(capnp_err!($x))
    };
}

macro_rules! capnp_err {
    ($x:expr_2021) => {
        $x.map_err(|e| Error::failed(format!("{:#}", e)))
    };
}

macro_rules! new_root_span {
    ($name:expr_2021, $container_id:expr_2021) => {
        debug_span!(
            $name,
            container_id = $container_id,
            uuid = Uuid::new_v4().to_string().as_str()
        )
    };
}

/// capnp_text_list takes text_list as an input and outputs list of text.
macro_rules! capnp_text_list {
    ($x:expr_2021) => {
        pry!(pry!($x).iter().collect::<Result<Vec<_>, _>>())
    };
}

macro_rules! capnp_vec_str {
    ($x:expr_2021) => {
        pry!(
            capnp_text_list!($x)
                .iter()
                .map(|s| s.to_string())
                .collect::<Result<Vec<_>, _>>()
        )
    };
}

macro_rules! capnp_vec_path {
    ($x:expr_2021) => {
        pry!(
            capnp_text_list!($x)
                .iter()
                .map(|s| s.to_str().map(|x| PathBuf::from(x)))
                .collect::<Result<Vec<_>, _>>()
        )
    };
}

#[allow(refining_impl_trait_reachable)]
impl conmon::Server for Server {
    /// Retrieve version information from the server.
    fn version(
        self: Rc<Server>,
        params: conmon::VersionParams,
        mut results: conmon::VersionResults,
    ) -> Promise<(), capnp::Error> {
        debug!("Got a version request");
        let req = pry!(pry!(params.get()).get_request());

        let span = debug_span!("version", uuid = Uuid::new_v4().to_string().as_str());
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

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
        self: Rc<Server>,
        params: conmon::CreateContainerParams,
        mut results: conmon::CreateContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry!(pry!(req.get_id()).to_string());

        let span = new_root_span!("create_container", id.as_str());
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

        let cleanup_cmd: Vec<String> = capnp_vec_str!(req.get_cleanup_cmd());

        debug!("Got a create container request");

        let log_drivers = pry!(req.get_log_drivers());
        let container_log = pry_err!(ContainerLog::from(log_drivers));
        let mut container_io =
            pry_err!(ContainerIO::new(req.get_terminal(), container_log.clone()));

        let bundle_path = Path::new(pry!(pry!(req.get_bundle_path()).to_str()));
        let pidfile = bundle_path.join("pidfile");
        debug!("PID file is {}", pidfile.display());

        let child_reaper = self.reaper().clone();
        let global_args = pry!(req.get_global_args());
        let command_args = pry!(req.get_command_args());
        let cgroup_manager = pry!(req.get_cgroup_manager());
        let args = GenerateRuntimeArgs {
            config: self.config(),
            id: &id,
            container_io: &container_io,
            pidfile: &pidfile,
            cgroup_manager,
        };
        let args = pry_err!(args.create_args(bundle_path, global_args, command_args));
        let stdin = req.get_stdin();
        let runtime = self.config().runtime().clone();
        let exit_paths = capnp_vec_path!(req.get_exit_paths());
        let oom_exit_paths = capnp_vec_path!(req.get_oom_exit_paths());
        let env_vars = pry!(req.get_env_vars().and_then(capnp_util::into_map));

        let additional_fds = pry_err!(self.fd_socket().take_all(pry!(req.get_additional_fds())));
        let leak_fds = pry_err!(self.fd_socket().take_all(pry!(req.get_leak_fds())));

        Promise::from_future(
            async move {
                capnp_err!(container_log.write().await.init().await)?;

                let (grandchild_pid, token) = capnp_err!(match child_reaper
                    .create_child(
                        runtime,
                        args,
                        stdin,
                        &mut container_io,
                        &pidfile,
                        env_vars,
                        additional_fds,
                    )
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
                capnp_err!(child_reaper.watch_grandchild(child, leak_fds))?;

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
        self: Rc<Server>,
        params: conmon::ExecSyncContainerParams,
        mut results: conmon::ExecSyncContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry!(pry!(req.get_id()).to_string());

        let span = new_root_span!("exec_sync_container", id.as_str());
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

        let timeout = req.get_timeout_sec();

        let pidfile = pry_err!(ContainerIO::temp_file_name(
            Some(self.config().runtime_dir()),
            "exec_sync",
            "pid"
        ));

        debug!("Got exec sync container request with timeout {}", timeout);

        let runtime = self.config().runtime().clone();
        let child_reaper = self.reaper().clone();

        let logger = ContainerLog::new();
        let mut container_io = pry_err!(ContainerIO::new(req.get_terminal(), logger));

        let command = pry!(req.get_command());
        let env_vars = pry!(req.get_env_vars().and_then(capnp_util::into_map));
        let cgroup_manager = pry!(req.get_cgroup_manager());

        let args = GenerateRuntimeArgs {
            config: self.config(),
            id: &id,
            container_io: &container_io,
            pidfile: &pidfile,
            cgroup_manager,
        };
        let args = pry_err!(args.exec_sync_args(command));

        Promise::from_future(
            async move {
                match child_reaper
                    .create_child(
                        &runtime,
                        &args,
                        false,
                        &mut container_io,
                        &pidfile,
                        env_vars,
                        vec![],
                    )
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

                        let mut exit_rx = capnp_err!(child_reaper.watch_grandchild(child, vec![]))?;

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
        self: Rc<Server>,
        params: conmon::AttachContainerParams,
        _: conmon::AttachContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry_err!(pry_err!(req.get_id()).to_str());

        let span = new_root_span!("attach_container", id);
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

        debug!("Got a attach container request",);

        let exec_session_id = pry_err!(pry_err!(req.get_exec_session_id()).to_str());
        if !exec_session_id.is_empty() {
            debug!("Using exec session id {}", exec_session_id);
        }

        let socket_path = pry!(pry!(req.get_socket_path()).to_string());
        let child = pry_err!(self.reaper().get(id));
        let stop_after_stdin_eof = req.get_stop_after_stdin_eof();

        Promise::from_future(
            async move {
                capnp_err!(
                    child
                        .io()
                        .attach()
                        .await
                        .add(&socket_path, child.token().clone(), stop_after_stdin_eof)
                        .await
                )
            }
            .instrument(debug_span!("promise")),
        )
    }

    /// Rotate all log drivers for a running container.
    fn reopen_log_container(
        self: Rc<Server>,
        params: conmon::ReopenLogContainerParams,
        _: conmon::ReopenLogContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry_err!(pry_err!(req.get_id()).to_str());

        let span = new_root_span!("reopen_log_container", id);
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

        debug!("Got a reopen container log request");

        let child = pry_err!(self.reaper().get(id));

        Promise::from_future(
            async move { capnp_err!(child.io().logger().await.write().await.reopen().await) }
                .instrument(debug_span!("promise")),
        )
    }

    /// Adjust the window size of a container running inside of a terminal.
    fn set_window_size_container(
        self: Rc<Server>,
        params: conmon::SetWindowSizeContainerParams,
        _: conmon::SetWindowSizeContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry_err!(pry_err!(req.get_id()).to_str());

        let span = new_root_span!("set_window_size_container", id);
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

        debug!("Got a set window size container request");

        let child = pry_err!(self.reaper().get(id));
        let width = req.get_width();
        let height = req.get_height();

        Promise::from_future(
            async move { capnp_err!(child.io().resize(width, height).await) }
                .instrument(debug_span!("promise")),
        )
    }

    /// Create a new set of namespaces.
    fn create_namespaces(
        self: Rc<Server>,
        params: conmon::CreateNamespacesParams,
        mut results: conmon::CreateNamespacesResults,
    ) -> Promise<(), capnp::Error> {
        debug!("Got a create namespaces request");
        let req = pry!(pry!(params.get()).get_request());
        let pod_id = pry_err!(pry_err!(req.get_pod_id()).to_str());

        if pod_id.is_empty() {
            return Promise::err(Error::failed("no pod ID provided".into()));
        }

        let span = new_root_span!("create_namespaces", pod_id);
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

        let pause = pry_err!(Pause::init_shared(
            pry!(pry!(req.get_base_path()).to_str()),
            pod_id,
            pry!(req.get_namespaces()),
            capnp_vec_str!(req.get_uid_mappings()),
            capnp_vec_str!(req.get_gid_mappings()),
        ));

        let response = results.get().init_response();
        let mut namespaces =
            response.init_namespaces(pry_err!(pause.namespaces().len().try_into()));

        for (idx, namespace) in pause.namespaces().iter().enumerate() {
            let mut ns = namespaces.reborrow().get(pry_err!(idx.try_into()));
            ns.set_path(
                namespace
                    .path(pause.base_path(), pod_id)
                    .display()
                    .to_string(),
            );
            ns.set_type(namespace.to_capnp_namespace());
        }

        Promise::ok(())
    }

    fn start_fd_socket(
        self: Rc<Server>,
        params: conmon::StartFdSocketParams,
        mut results: conmon::StartFdSocketResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());

        let span = debug_span!(
            "start_fd_socket",
            uuid = Uuid::new_v4().to_string().as_str()
        );
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

        debug!("Got a start fd socket request");

        let path = self.config().fd_socket();
        let fd_socket = self.fd_socket().clone();

        Promise::from_future(
            async move {
                let path = capnp_err!(fd_socket.start(path).await)?;

                let mut resp = results.get().init_response();
                resp.set_path(capnp_err!(path.to_str().context("fd_socket path to str"))?);

                Ok(())
            }
            .instrument(debug_span!("promise")),
        )
    }

    fn serve_exec_container(
        self: Rc<Server>,
        params: conmon::ServeExecContainerParams,
        mut results: conmon::ServeExecContainerResults,
    ) -> Promise<(), capnp::Error> {
        debug!("Got a serve exec container request");
        let req = pry!(pry!(params.get()).get_request());

        let span = debug_span!(
            "serve_exec_container",
            uuid = Uuid::new_v4().to_string().as_str()
        );
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

        let id = pry_err!(pry_err!(req.get_id()).to_string());

        // Validate that the container actually exists
        pry_err!(self.reaper().get(&id));

        let command = capnp_vec_str!(req.get_command());
        let (tty, stdin, stdout, stderr) = (
            req.get_tty(),
            req.get_stdin(),
            req.get_stdout(),
            req.get_stderr(),
        );

        let streaming_server = self.streaming_server().clone();
        let child_reaper = self.reaper().clone();
        let container_io = pry_err!(ContainerIO::new(tty, ContainerLog::new()));
        let config = self.config().clone();
        let cgroup_manager = pry!(req.get_cgroup_manager());

        Promise::from_future(
            async move {
                capnp_err!(
                    streaming_server
                        .write()
                        .await
                        .start_if_required()
                        .await
                        .context("start streaming server if required")
                )?;

                let url = streaming_server
                    .read()
                    .await
                    .exec_url(
                        child_reaper,
                        container_io,
                        config,
                        cgroup_manager,
                        id,
                        command,
                        stdin,
                        stdout,
                        stderr,
                    )
                    .await;

                results.get().init_response().set_url(&url);
                Ok(())
            }
            .instrument(debug_span!("promise")),
        )
    }

    fn serve_attach_container(
        self: Rc<Server>,
        params: conmon::ServeAttachContainerParams,
        mut results: conmon::ServeAttachContainerResults,
    ) -> Promise<(), capnp::Error> {
        debug!("Got a serve attach container request");
        let req = pry!(pry!(params.get()).get_request());

        let span = debug_span!(
            "serve_attach_container",
            uuid = Uuid::new_v4().to_string().as_str()
        );
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

        let id = pry_err!(pry_err!(req.get_id()).to_str());
        let (stdin, stdout, stderr) = (req.get_stdin(), req.get_stdout(), req.get_stderr());

        let streaming_server = self.streaming_server().clone();
        let child = pry_err!(self.reaper().get(id));

        Promise::from_future(
            async move {
                capnp_err!(
                    streaming_server
                        .write()
                        .await
                        .start_if_required()
                        .await
                        .context("start streaming server")
                )?;

                let url = streaming_server
                    .read()
                    .await
                    .attach_url(child, stdin, stdout, stderr)
                    .await;

                results.get().init_response().set_url(&url);
                Ok(())
            }
            .instrument(debug_span!("promise")),
        )
    }

    fn serve_port_forward_container(
        self: Rc<Server>,
        params: conmon::ServePortForwardContainerParams,
        mut results: conmon::ServePortForwardContainerResults,
    ) -> Promise<(), capnp::Error> {
        debug!("Got a serve port forward container request");
        let req = pry!(pry!(params.get()).get_request());

        let span = debug_span!(
            "serve_port_forward_container",
            uuid = Uuid::new_v4().to_string().as_str()
        );
        let _enter = span.enter();
        pry_err!(Telemetry::set_parent_context(pry!(req.get_metadata())));

        let net_ns_path = pry_err!(pry_err!(req.get_net_ns_path()).to_string());
        let streaming_server = self.streaming_server().clone();

        Promise::from_future(
            async move {
                capnp_err!(
                    streaming_server
                        .write()
                        .await
                        .start_if_required()
                        .await
                        .context("start streaming server if required")
                )?;

                let url = streaming_server
                    .read()
                    .await
                    .port_forward_url(net_ns_path)
                    .await;

                results.get().init_response().set_url(&url);
                Ok(())
            }
            .instrument(debug_span!("promise")),
        )
    }
}
