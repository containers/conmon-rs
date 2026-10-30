//! The Cap'n Proto RPC interface of the server, one method for each call of
//! `proto/conmon.capnp`.

use crate::conmon_capnp::conmon;
#[cfg(feature = "tracing")]
use crate::telemetry::Telemetry;
use crate::{
    capnp_util,
    child::{Child, ChildKind, sleep_until},
    child_reaper::{EXIT_CODE_NOT_STARTED, EXIT_CODE_TIMED_OUT, RuntimeCall},
    container_io::{ContainerIO, SharedContainerIO},
    container_log::ContainerLog,
    notify::NotifySocket,
    pause::Pause,
    process::Process,
    random_id::RandomId,
    runtime_args::GenerateRuntimeArgs,
    server::Server,
    streaming_server::{AttachSession, ExecSession, StreamingServer},
    version::Version,
};
use anyhow::Context;
use capnp::{Error, capability::Promise};
use capnp_rpc::pry;
use futures::FutureExt;
use nix::{sys::signal::Signal, unistd::Pid};
use std::{
    path::{Path, PathBuf},
    process,
    rc::Rc,
    time::Duration,
};
use tokio::{
    select,
    sync::{RwLock, oneshot},
    task::{self, JoinHandle},
    time::Instant,
};
use tracing::{Instrument, Span, debug, debug_span, error};

macro_rules! pry_err {
    ($x:expr_2021) => {
        pry!(capnp_err!($x))
    };
}

macro_rules! capnp_err {
    ($x:expr_2021) => {
        $x.map_err(|e| Error::failed(format!("{e:#}")))
    };
}

/// The result of a call which runs in a task of its own, a panic of the task
/// fails the call like an error of it.
async fn task_result<T>(task: JoinHandle<anyhow::Result<T>>) -> Result<T, Error> {
    capnp_err!(capnp_err!(task.await)?)
}

/// Make the context the client sent in the metadata of a request the parent
/// of its span, before the span got entered. Metadata which cannot be read
/// does not fail the request, it only loses the parent.
#[cfg(feature = "tracing")]
fn set_parent_context(
    span: &Span,
    metadata: capnp::Result<capnp::struct_list::Reader<'_, conmon::text_text_map_entry::Owned>>,
) -> anyhow::Result<()> {
    Telemetry::set_parent_context(span, metadata?)
}

#[cfg(not(feature = "tracing"))]
fn set_parent_context(
    _span: &Span,
    _metadata: capnp::Result<capnp::struct_list::Reader<'_, conmon::text_text_map_entry::Owned>>,
) -> anyhow::Result<()> {
    Ok(())
}

/// Enter the root span of a request for the rest of the handler, with the
/// parent context the client sent.
macro_rules! enter_request {
    ($req:expr_2021, $name:literal $(, $container_id:expr_2021)?) => {
        let span = debug_span!(
            $name,
            $(container_id = $container_id,)?
            request_id = %RandomId::new()
        );
        // Entering starts the span, its parent is fixed then.
        let parent = set_parent_context(&span, $req.get_metadata());
        let _enter = span.enter();
        if let Err(e) = parent {
            debug!("Unable to set the parent context of the request: {e:#}");
        }
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

/// The result of an exec_sync.
struct ExecSyncOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: i32,
    timed_out: bool,
}

impl Server {
    /// The deadline of an exec_sync, none without a timeout or if it does not
    /// fit, which is no timeout either.
    fn exec_deadline(timeout_sec: u64) -> Option<Instant> {
        if timeout_sec == 0 {
            return None;
        }
        Instant::now().checked_add(Duration::from_secs(timeout_sec))
    }

    /// How long the runtime of an exec may take beyond its timeout before it
    /// gets killed. It only starts the process, so this bounds an exec
    /// without a timeout as well.
    const EXEC_RUNTIME_GRACE: Duration = Duration::from_secs(60);

    /// When the runtime of an exec with the `timeout` gets killed. A runtime
    /// which hangs must not keep its call in progress forever, which defers
    /// the reaping of orphans and delays the shutdown.
    pub(crate) fn exec_runtime_deadline(timeout: Option<Instant>) -> Option<Instant> {
        timeout
            .unwrap_or_else(Instant::now)
            .checked_add(Self::EXEC_RUNTIME_GRACE)
    }

    /// How long the runtime of a create may take before it gets killed, well
    /// above the time CRI-O gives the creation of a container, so that a
    /// runtime which hangs does not keep its call in progress forever.
    const CREATE_RUNTIME_TIMEOUT: Duration = Duration::from_secs(10 * 60);

    /// The idle timeout of a streaming connection, none for 0.
    fn idle_timeout(timeout_sec: u64) -> Option<Duration> {
        (timeout_sec != 0).then(|| Duration::from_secs(timeout_sec))
    }

    /// Start the streaming server if required and return the URL of the
    /// session.
    async fn streaming_url(
        streaming_server: &RwLock<StreamingServer>,
        request: StreamingRequest,
    ) -> Result<String, Error> {
        capnp_err!(
            streaming_server
                .write()
                .await
                .start_if_required()
                .await
                .context("start streaming server")
        )?;

        let server = streaming_server.read().await;
        // Without a context: clients match the start of the message, like the
        // one of too many waiting sessions, see `ErrTooManyRequests` of the Go
        // client.
        capnp_err!(match request {
            StreamingRequest::Exec(session) => server.exec_url(session).await,
            StreamingRequest::Attach(session) => server.attach_url(session).await,
            StreamingRequest::PortForward {
                netns_path,
                idle_timeout,
            } => server.port_forward_url(netns_path, idle_timeout).await,
        })
    }
}

/// A session for the streaming server, see [`Server::streaming_url`].
enum StreamingRequest {
    Exec(ExecSession),
    Attach(AttachSession),
    PortForward {
        netns_path: PathBuf,
        idle_timeout: Option<Duration>,
    },
}

#[expect(
    refining_impl_trait_reachable,
    reason = "the methods return the concrete capnp promises, the trait any future"
)]
impl conmon::Server for Server {
    /// Retrieve version information from the server.
    fn version(
        self: Rc<Server>,
        params: conmon::VersionParams,
        mut results: conmon::VersionResults,
    ) -> Promise<(), capnp::Error> {
        debug!("Got a version request");
        let req = pry!(pry!(params.get()).get_request());

        enter_request!(req, "version");

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
        let mut features =
            response.init_features(pry_err!(u32::try_from(version.features().len())));
        for (i, feature) in (0..).zip(version.features()) {
            features.set(i, feature);
        }

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

        enter_request!(req, "create_container", id.as_str());

        let cleanup_cmd: Vec<String> = capnp_vec_str!(req.get_cleanup_cmd());

        debug!("Got a create container request");

        let log_drivers = pry!(req.get_log_drivers());
        let container_log = pry_err!(ContainerLog::from_request(&id, log_drivers));
        let mut container_io =
            pry_err!(ContainerIO::new(req.get_terminal(), container_log.clone()));

        let bundle_path = Path::new(pry!(pry!(req.get_bundle_path()).to_str()));
        pry_err!(capnp_util::check_absolute("bundle path", bundle_path));
        let pidfile = bundle_path.join("pidfile");
        debug!("PID file is {}", pidfile.display());

        let child_reaper = self.reaper().clone();
        let global_args = capnp_vec_str!(req.get_global_args());
        let command_args = capnp_vec_str!(req.get_command_args());
        let cgroup_manager = pry!(req.get_cgroup_manager());
        let args = GenerateRuntimeArgs {
            config: self.config(),
            id: &id,
            container_io: &container_io,
            pidfile: &pidfile,
            cgroup_manager,
        };
        let args = pry_err!(args.create_args(bundle_path, &global_args, &command_args));
        let stdin = req.get_stdin();
        let runtime = self.config().runtime().to_path_buf();
        let exit_paths = capnp_vec_path!(req.get_exit_paths());
        let oom_exit_paths = capnp_vec_path!(req.get_oom_exit_paths());
        for path in exit_paths.iter().chain(&oom_exit_paths) {
            pry_err!(capnp_util::check_absolute("exit path", path));
        }
        let env_vars = pry!(req.get_env_vars().and_then(capnp_util::into_map));

        // The runtime mounts the socket directory, so it has to exist before
        // the runtime runs.
        let sdnotify_socket = pry!(pry!(req.get_sdnotify_socket()).to_str());
        let notify_socket = if sdnotify_socket.is_empty() {
            None
        } else {
            Some(pry_err!(NotifySocket::bind(bundle_path, sdnotify_socket)))
        };

        // Take both lists at once and last, a failure leaves all descriptors
        // in place for a retry.
        let additional_slots = pry!(req.get_additional_fds());
        let leak_slots = pry!(req.get_leak_fds());
        let mut additional_fds = pry_err!(
            self.fd_socket()
                .take_all(additional_slots.iter().chain(leak_slots.iter()))
        );
        let leak_fds = additional_fds.split_off(additional_slots.len() as usize);

        let runtime_deadline = Instant::now().checked_add(Self::CREATE_RUNTIME_TIMEOUT);
        Promise::from_future(
            async move {
                // capnp-rpc drops this future once the client disconnects or
                // gives up on the call. The runtime may be creating the
                // container right then, so finish and register it in a task of
                // its own, otherwise nothing would watch the container.
                let create = task::spawn_local(
                    async move {
                        container_log.write().await.init().await?;

                        let (grandchild_pid, token, call) = match child_reaper
                            .create_child(
                                RuntimeCall {
                                    stdin,
                                    deadline: runtime_deadline,
                                    env_vars,
                                    additional_fds,
                                    ..RuntimeCall::new(&runtime, &args, &pidfile)
                                },
                                &mut container_io,
                            )
                            .await
                        {
                            Ok(res) => res,
                            Err(e) => return Err(container_io.runtime_error(e).await),
                        };

                        // The stdio channels are needed up to this point to
                        // report the runtime's stderr on a failed create (see
                        // above). Afterwards nothing consumes them, attach
                        // sessions use the attach broadcast, so drop them
                        // instead of retaining output for the lifetime of the
                        // container.
                        container_io.drop_stdio();

                        // register grandchild with server
                        let io = SharedContainerIO::new(container_io);
                        let child = Child {
                            id: id.into(),
                            kind: ChildKind::Container,
                            pid: grandchild_pid,
                            exit_paths,
                            oom_exit_paths,
                            timeout: None,
                            io,
                            cleanup_cmd,
                            token: token.clone(),
                        };
                        child_reaper.watch_grandchild(child, leak_fds, call)?;

                        // Only now does the container exit cancel the token, a
                        // failure above drops the socket instead.
                        if let Some(notify_socket) = notify_socket {
                            notify_socket.spawn(token);
                        }
                        Ok(grandchild_pid)
                    }
                    // Logged here as well, the client may be gone.
                    .map(|res: anyhow::Result<Pid>| {
                        res.inspect_err(|e| error!("Unable to create container: {e:#}"))
                    })
                    .instrument(Span::current()),
                );
                let grandchild_pid = task_result(create).await?;

                results
                    .get()
                    .init_response()
                    .set_container_pid(capnp_err!(u32::try_from(grandchild_pid.as_raw()))?);
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

        enter_request!(req, "exec_sync_container", id.as_str());

        // The timeout starts now, the runtime may hang before the process
        // even starts.
        let time_to_timeout = Self::exec_deadline(req.get_timeout_sec());

        let pidfile =
            ContainerIO::temp_file(Some(self.config().runtime_dir()), "exec_sync", ".pid");

        debug!(
            "Got exec sync container request with timeout {}",
            req.get_timeout_sec()
        );

        let runtime = self.config().runtime().to_path_buf();
        let child_reaper = self.reaper().clone();

        let logger = ContainerLog::new();
        let mut container_io = pry_err!(ContainerIO::new(req.get_terminal(), logger));

        let command = capnp_vec_str!(req.get_command());
        let env_vars = pry!(req.get_env_vars().and_then(capnp_util::into_map));
        let cgroup_manager = pry!(req.get_cgroup_manager());

        let args = GenerateRuntimeArgs {
            config: self.config(),
            id: &id,
            container_io: &container_io,
            pidfile: pidfile.path(),
            cgroup_manager,
        };
        let args = pry_err!(args.exec_sync_args(&command));

        Promise::from_future(
            async move {
                // See create_container, the exec has to be watched and its
                // output drained even if the client goes away.
                // The runtime may hang, it gets killed with a grace on top of
                // the timeout, or after the grace without one. The client gets
                // its timeout right away, the task kills the process the
                // runtime started once it finished.
                let runtime_deadline = Self::exec_runtime_deadline(time_to_timeout);
                let (started_tx, mut started_rx) = oneshot::channel();
                let exec = task::spawn_local(
                    async move {
                        // Read while the runtime runs, see
                        // `ContainerIO::collect_stdio`.
                        container_io.collect_stdio(true, true)?;
                        let res = child_reaper
                            .create_child(
                                RuntimeCall {
                                    deadline: runtime_deadline,
                                    env_vars,
                                    ..RuntimeCall::new(&runtime, &args, pidfile.path())
                                },
                                &mut container_io,
                            )
                            .await;
                        let _ = started_tx.send(());
                        let (grandchild_pid, token, call) = match res {
                            Ok(res) => res,
                            Err(e) => {
                                let timed_out =
                                    time_to_timeout.is_some_and(|t| Instant::now() >= t);
                                let e = container_io.runtime_error(e).await;
                                error!("Unable to create child: {e:#}");
                                // The client only sees the response, tell it why.
                                return Ok(ExecSyncOutput {
                                    stdout: vec![],
                                    stderr: format!("{e:#}").into_bytes(),
                                    exit_code: if timed_out {
                                        EXIT_CODE_TIMED_OUT
                                    } else {
                                        EXIT_CODE_NOT_STARTED
                                    },
                                    timed_out,
                                });
                            }
                        };

                        // register grandchild with server, a timeout which
                        // already passed kills it right away
                        let io = SharedContainerIO::new(container_io);
                        // Not reaped before the registration below, so the
                        // PID is still its own.
                        let process = Process::open(grandchild_pid);
                        let child = Child {
                            id: id.into(),
                            kind: ChildKind::Exec,
                            pid: grandchild_pid,
                            exit_paths: vec![],
                            oom_exit_paths: vec![],
                            timeout: time_to_timeout,
                            io: io.clone(),
                            cleanup_cmd: vec![],
                            token: token.clone(),
                        };
                        let _stop_output = io.attach().await.stop_output_on_exit(token.clone());
                        let mut exit_rx = child_reaper.watch_grandchild(child, vec![], call)?;

                        // Reads until the output ended or the drain after the
                        // exit gave up, under the write lock of the IO, the
                        // read loops get stopped only after that.
                        let (stdout, stderr, timed_out) =
                            io.read_all_with_timeout(time_to_timeout, &token).await?;
                        let exit_data = match exit_rx.recv().await {
                            Ok(exit_data) => exit_data,
                            // The watcher got aborted, nothing ends the
                            // process and its output otherwise, like for a
                            // streaming exec.
                            Err(e) => {
                                if let Err(e) = process.signal_group(Signal::SIGKILL) {
                                    debug!("Not killing exec process {grandchild_pid}: {e}");
                                }
                                io.attach().await.stop_output();
                                return Err(e).context("receive the exit of the exec");
                            }
                        };
                        Ok::<_, anyhow::Error>(ExecSyncOutput {
                            stdout,
                            stderr,
                            exit_code: exit_data.exit_code,
                            timed_out: timed_out || exit_data.timed_out,
                        })
                    }
                    // Logged here as well, the client may have its reply
                    // already.
                    .map(|res: anyhow::Result<ExecSyncOutput>| {
                        res.inspect_err(|e| error!("Unable to run exec: {e:#}"))
                    })
                    .instrument(Span::current()),
                );

                let started = select! {
                    _ = &mut started_rx => true,
                    () = sleep_until(time_to_timeout) => false,
                };
                let output = if started {
                    task_result(exec).await?
                } else {
                    debug!("Runtime did not start the exec before its timeout");
                    ExecSyncOutput {
                        stdout: vec![],
                        stderr: b"timed out waiting for the runtime to start the process".to_vec(),
                        exit_code: EXIT_CODE_TIMED_OUT,
                        timed_out: true,
                    }
                };

                let mut resp = results.get().init_response();
                resp.set_stdout(&output.stdout);
                resp.set_stderr(&output.stderr);
                resp.set_exit_code(output.exit_code);
                resp.set_timed_out(output.timed_out);
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

        enter_request!(req, "attach_container", id);

        debug!("Got an attach container request");

        let exec_session_id = pry_err!(pry_err!(req.get_exec_session_id()).to_str());
        if !exec_session_id.is_empty() {
            debug!("Using exec session id {}", exec_session_id);
        }

        let socket_path = pry!(pry!(req.get_socket_path()).to_string());
        pry_err!(capnp_util::check_absolute(
            "socket path",
            Path::new(&socket_path)
        ));
        let child = pry_err!(self.reaper().get(id));
        let stop_after_stdin_eof = req.get_stop_after_stdin_eof();

        Promise::from_future(
            async move {
                capnp_err!(child.io().attach().await.add(
                    &socket_path,
                    child.token().clone(),
                    stop_after_stdin_eof
                ))
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

        enter_request!(req, "reopen_log_container", id);

        debug!("Got a reopen container log request");

        let child = pry_err!(self.reaper().get(id));

        Promise::from_future(
            async move {
                // A dropped call must not stop a reopen halfway, between the
                // write of the old file and the open of the new one.
                let reopen = task::spawn_local(
                    async move { child.io().logger().await.write().await.reopen().await }
                        .instrument(Span::current()),
                );
                task_result(reopen).await
            }
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

        enter_request!(req, "set_window_size_container", id);

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

        enter_request!(req, "create_namespaces", pod_id);

        let base_path = pry!(pry!(req.get_base_path()).to_string());
        let pod_id = pod_id.to_owned();
        let namespaces = pry_err!(Pause::parse_namespaces(pry!(req.get_namespaces())));
        let uid_mappings = capnp_vec_str!(req.get_uid_mappings());
        let gid_mappings = capnp_vec_str!(req.get_gid_mappings());

        Promise::from_future(
            async move {
                let pause = capnp_err!(
                    Pause::init_shared(&base_path, &pod_id, namespaces, uid_mappings, gid_mappings)
                        .await
                )?;

                let response = results.get().init_response();
                let mut namespaces =
                    response.init_namespaces(capnp_err!(pause.namespaces().len().try_into())?);

                for (idx, namespace) in pause.namespaces().iter().enumerate() {
                    let mut ns = namespaces.reborrow().get(capnp_err!(idx.try_into())?);
                    ns.set_path(
                        namespace
                            .path(pause.base_path(), &pod_id)
                            .display()
                            .to_string(),
                    );
                    ns.set_type(namespace.to_capnp_namespace());
                }

                Ok(())
            }
            .instrument(debug_span!("promise")),
        )
    }

    fn start_fd_socket(
        self: Rc<Server>,
        params: conmon::StartFdSocketParams,
        mut results: conmon::StartFdSocketResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());

        enter_request!(req, "start_fd_socket");

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

        let id = pry_err!(pry_err!(req.get_id()).to_string());

        enter_request!(req, "serve_exec_container", id.as_str());

        // Validate that the container actually exists
        pry_err!(self.reaper().get(&id));

        let command = capnp_vec_str!(req.get_command());
        let (tty, stdin, stdout, stderr) = (
            req.get_tty(),
            req.get_stdin(),
            req.get_stdout(),
            req.get_stderr(),
        );
        let idle_timeout = Server::idle_timeout(req.get_idle_timeout_sec());

        let streaming_server = self.streaming_server().clone();
        let child_reaper = self.reaper().clone();
        let config = self.config().clone();
        let cgroup_manager = pry!(req.get_cgroup_manager());

        Promise::from_future(
            async move {
                let url = Self::streaming_url(
                    &streaming_server,
                    StreamingRequest::Exec(ExecSession {
                        child_reaper,
                        tty,
                        server_config: config,
                        cgroup_manager,
                        container_id: id.into_boxed_str(),
                        command,
                        stdin,
                        stdout,
                        stderr,
                        idle_timeout,
                    }),
                )
                .await?;

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

        let id = pry_err!(pry_err!(req.get_id()).to_str());

        enter_request!(req, "serve_attach_container", id);

        let (stdin, stdout, stderr) = (req.get_stdin(), req.get_stdout(), req.get_stderr());
        let stop_after_stdin_eof = req.get_stop_after_stdin_eof();
        let idle_timeout = Server::idle_timeout(req.get_idle_timeout_sec());

        let streaming_server = self.streaming_server().clone();
        let child = pry_err!(self.reaper().get(id));
        let id: Box<str> = id.into();

        Promise::from_future(
            async move {
                let url = Self::streaming_url(
                    &streaming_server,
                    StreamingRequest::Attach(AttachSession {
                        container_id: id,
                        child,
                        stdin,
                        stdout,
                        stderr,
                        stop_after_stdin_eof,
                        idle_timeout,
                    }),
                )
                .await?;

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

        enter_request!(req, "serve_port_forward_container");

        let netns_path = pry_err!(pry_err!(req.get_net_ns_path()).to_str());
        if netns_path.is_empty() {
            return Promise::err(Error::failed("network namespace path is empty".into()));
        }
        let netns_path = PathBuf::from(netns_path);
        pry_err!(capnp_util::check_absolute(
            "network namespace path",
            &netns_path
        ));
        let idle_timeout = Server::idle_timeout(req.get_idle_timeout_sec());
        let streaming_server = self.streaming_server().clone();

        Promise::from_future(
            async move {
                let url = Self::streaming_url(
                    &streaming_server,
                    StreamingRequest::PortForward {
                        netns_path,
                        idle_timeout,
                    },
                )
                .await?;

                results.get().init_response().set_url(&url);
                Ok(())
            }
            .instrument(debug_span!("promise")),
        )
    }
}

#[cfg(test)]
mod tests;
