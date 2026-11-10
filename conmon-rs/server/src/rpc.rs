//! The Cap'n Proto RPC interface of the server, one method for each call of
//! `proto/conmon.capnp`.

use crate::conmon_capnp::conmon;
#[cfg(feature = "tracing")]
use crate::telemetry;
use crate::{
    child_reaper::{
        Child, ChildKind, ChildReaper, EXIT_CODE_NOT_STARTED, EXIT_CODE_TIMED_OUT, ExitChannelData,
        RuntimeCall, StartedChild, WatchedExec,
    },
    container_io::{ContainerIO, SharedContainerIO},
    container_log::ContainerLog,
    exec_start::ExecStart,
    fs_util,
    random_id::RandomId,
    runtime_args::{GenerateRuntimeArgs, check_id},
    server::Server,
    streaming_server::{AttachSession, ExecSession, SessionCommon},
    util::sleep_until,
    version::Version,
};
use anyhow::Context;
use capnp::{Error, capability::Promise};
use capnp_rpc::pry;
use futures::FutureExt;
use nix::sys::signal::Signal;
use std::{
    path::{Path, PathBuf},
    process,
    rc::Rc,
    time::Duration,
};
use tokio::{
    select,
    sync::{broadcast, oneshot},
    task::{self, JoinHandle},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, debug, debug_span, error};

macro_rules! pry_err {
    ($x:expr) => {
        pry!(capnp_err!($x))
    };
}

macro_rules! capnp_err {
    ($x:expr) => {
        $x.map_err(|e| Error::failed(format!("{e:#}")))
    };
}

/// The result of a call which runs in a task of its own. A task which
/// panicked fails the call like an error, but only where panics unwind, like
/// in the tests: release builds abort on a panic.
async fn task_result<T>(task: JoinHandle<anyhow::Result<T>>) -> Result<T, Error> {
    capnp_err!(capnp_err!(task.await)?)
}

/// Run the part of a call which must finish also if capnp-rpc drops the
/// call, like once the client disconnects, in a task of its own, see
/// [`task_result`]. Its error gets logged here as well, the client may be
/// gone or have its reply already.
fn spawn_logged<T: 'static>(
    what: &'static str,
    fut: impl Future<Output = anyhow::Result<T>> + 'static,
) -> JoinHandle<anyhow::Result<T>> {
    task::spawn_local(
        fut.map(move |res| res.inspect_err(|e| error!("Unable to {what}: {e:#}")))
            .instrument(Span::current()),
    )
}

/// Make the context the client sent in the metadata of a request the parent
/// of its span, before the span got entered. Metadata which cannot be read
/// does not fail the request, it only loses the parent.
#[cfg(feature = "tracing")]
fn set_parent_context(
    span: &Span,
    metadata: capnp::Result<capnp::struct_list::Reader<'_, conmon::text_text_map_entry::Owned>>,
) -> anyhow::Result<()> {
    telemetry::set_parent_context(span, metadata?)
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
    ($req:expr, $name:literal $(, $container_id:expr)?) => {
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

/// Read a text list of a request, like into strings or paths.
fn text_list<'a, T: From<&'a str>>(
    list: capnp::Result<capnp::text_list::Reader<'a>>,
) -> capnp::Result<Vec<T>> {
    list?
        .iter()
        .map(|text| Ok(text?.to_str()?.into()))
        .collect()
}

/// Fail a request which sets one of the retired fields, which an older
/// client may send, named like they were, see the schema. Ignoring one would
/// change what the runtime does, like its root through `globalArgs` or the
/// descriptors it inherits through `additionalFds`, or the journal entries
/// of a container through the `tag` of a log driver. Empty ones, which the
/// older clients send by default, pass. A client since v1.1.0 checks the
/// feature of the `sdnotifySocket` before it sends one.
fn reject_retired<const N: usize>(method: &str, fields: [(&str, bool); N]) -> Result<(), Error> {
    match fields.iter().find(|(_, empty)| !empty) {
        Some((field, _)) => Err(Error::unimplemented(format!(
            "the {field} of {method} is not supported by this server"
        ))),
        None => Ok(()),
    }
}

/// The result of an exec_sync.
struct ExecSyncOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: i32,
    timed_out: bool,
}

impl ExecSyncOutput {
    /// What the response keeps free for the framing of the gRPC message
    /// which carries it, see [`ExecSyncOutput::MAX_SIZE`].
    const FRAMING_HEADROOM: usize = 64 * 1024;

    /// The most output an exec_sync returns, stdout and stderr together,
    /// like conmon. The response has to fit into the 16 MiB gRPC messages of
    /// CRI-O and the kubelet, which also carry the framing.
    const MAX_SIZE: usize = ContainerIO::MAX_STDIO_STREAM_SIZE - Self::FRAMING_HEADROOM;

    /// Cut the output to [`ExecSyncOutput::MAX_SIZE`]. Each stream keeps up
    /// to half of it, and more of what the other one leaves, so that a large
    /// output of one does not drop the few lines of the other, like an error
    /// on stderr.
    fn cap(stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) {
        let half = Self::MAX_SIZE / 2;
        stdout.truncate(half.max(Self::MAX_SIZE.saturating_sub(stderr.len())));
        stderr.truncate(Self::MAX_SIZE - stdout.len());
    }
}

/// The deadline of an exec_sync, none without a timeout or if it does not
/// fit, which is no timeout either.
fn exec_deadline(timeout_sec: u64) -> Option<Instant> {
    if timeout_sec == 0 {
        return None;
    }
    Instant::now().checked_add(Duration::from_secs(timeout_sec))
}

/// Read the output of an exec while waiting for its exit, and take the
/// result from the watcher alone. The watcher cancels the `token` once it
/// reaped the process or killed it at its timeout, the drain bounds the
/// reading after that. A deadline of the reading on its own would race the
/// one of the watcher, and report a timeout with the exit code of a process
/// which exited right at it, without its last output. A watcher which got
/// lost never cancels the token, nothing ends the process and its output
/// otherwise, like for a streaming exec, so it gets killed with `kill`.
/// Cancelling the token then stops the output, see
/// [`StopOutput`](crate::attach::StopOutput).
async fn exec_sync_output(
    io: &SharedContainerIO,
    token: &CancellationToken,
    mut exit_rx: broadcast::Receiver<ExitChannelData>,
    kill: impl FnOnce(),
) -> anyhow::Result<ExecSyncOutput> {
    let exit = async move {
        let res = exit_rx.recv().await;
        if res.is_err() {
            kill();
            token.cancel();
        }
        res.context("receive the exit of the exec")
    };
    // Reads until the output ended or the drain after the exit gave up, the
    // read loops get stopped only after that.
    let ((mut stdout, mut stderr), exit_data) = tokio::try_join!(io.read_all(token), exit)?;
    ExecSyncOutput::cap(&mut stdout, &mut stderr);
    Ok(ExecSyncOutput {
        stdout,
        stderr,
        exit_code: exit_data.exit_code,
        timed_out: exit_data.timed_out,
    })
}

/// The idle timeout of a streaming connection, none for 0.
fn idle_timeout(timeout_sec: u64) -> Option<Duration> {
    (timeout_sec != 0).then(|| Duration::from_secs(timeout_sec))
}

/// The result of a request for the URL of a streaming session. Without a
/// context: clients match the start of the message, like the one of too many
/// waiting sessions, see `ErrTooManyRequests` of the Go client.
fn streaming_result(res: anyhow::Result<String>) -> Result<String, Error> {
    capnp_err!(res)
}

#[expect(
    refining_impl_trait_internal,
    reason = "the methods return the concrete capnp promises, the trait any future"
)]
impl conmon::Server for Server {
    /// Retrieve version information from the server.
    fn version(
        self: Rc<Self>,
        params: conmon::VersionParams,
        mut results: conmon::VersionResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());

        enter_request!(req, "version");

        debug!("Got a version request");

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
        self: Rc<Self>,
        params: conmon::CreateContainerParams,
        mut results: conmon::CreateContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry!(pry!(req.get_id()).to_string());

        enter_request!(req, "create_container", id.as_str());

        debug!("Got a create container request");

        pry!(reject_retired(
            "createContainer",
            [
                (
                    "sdnotifySocket",
                    pry!(req.get_sdnotify_socket_old()).is_empty(),
                ),
                ("cleanupCmd", pry!(req.get_cleanup_cmd_old()).is_empty()),
                ("globalArgs", pry!(req.get_global_args_old()).is_empty()),
                ("envVars", pry!(req.get_env_vars_old()).is_empty()),
                (
                    "additionalFds",
                    pry!(req.get_additional_fds_old()).is_empty(),
                ),
                ("leakFds", pry!(req.get_leak_fds_old()).is_empty()),
            ],
        ));

        let log_drivers = pry!(req.get_log_drivers());
        for driver in log_drivers {
            pry!(reject_retired(
                "createContainer",
                [
                    (
                        "json log driver",
                        pry!(driver.get_type()) != conmon::log_driver::Type::JsonOld,
                    ),
                    ("tag of a log driver", pry!(driver.get_tag_old()).is_empty()),
                    (
                        "labels of a log driver",
                        pry!(driver.get_labels_old()).is_empty(),
                    ),
                ],
            ));
        }

        // The request gets checked before anything gets set up for it: a
        // rejected one must not bind a console socket and spawn its listener
        // for nothing.
        pry_err!(check_id(&id));
        let bundle_path = Path::new(pry!(pry!(req.get_bundle_path()).to_str()));
        pry_err!(fs_util::check_absolute("bundle path", bundle_path));
        let pidfile = bundle_path.join("pidfile");
        debug!("PID file is {}", pidfile.display());

        // An empty one, which the older clients send, is a create. A client
        // checks the `restore` feature before it sets one, see the schema.
        let (restore_path, action) = match pry!(pry!(req.get_restore_path()).to_str()) {
            "" => (None, "create container"),
            path => {
                let path = Path::new(path);
                pry_err!(fs_util::check_absolute("restore path", path));
                debug!("Restoring from checkpoint {}", path.display());
                (Some(path), "restore container")
            }
        };

        let exit_paths: Vec<PathBuf> = pry!(text_list(req.get_exit_paths()));
        let oom_exit_paths: Vec<PathBuf> = pry!(text_list(req.get_oom_exit_paths()));
        for path in &exit_paths {
            pry_err!(fs_util::check_absolute("exit path", path));
        }
        for path in &oom_exit_paths {
            pry_err!(fs_util::check_absolute("OOM exit path", path));
        }
        let container_log = pry_err!(ContainerLog::from_request(&id, log_drivers));

        let mut container_io =
            pry_err!(ContainerIO::new(req.get_terminal(), container_log.clone()));
        let child_reaper = self.reaper().clone();
        let command_args: Vec<String> = pry!(text_list(req.get_command_args()));
        let cgroup_manager = pry!(req.get_cgroup_manager());
        let args = GenerateRuntimeArgs {
            config: self.config(),
            id: &id,
            container_io: &container_io,
            pidfile: &pidfile,
            cgroup_manager,
        };
        let args = pry_err!(args.create_args(bundle_path, restore_path, &command_args));
        let stdin = req.get_stdin();
        let runtime = self.config().runtime().to_path_buf();

        let runtime_deadline = Instant::now() + ChildReaper::CREATE_RUNTIME_TIMEOUT;
        Promise::from_future(
            async move {
                // capnp-rpc drops this future once the client disconnects or
                // gives up on the call. The runtime may be creating or
                // restoring the container right then, so finish and register
                // it in a task of its own, otherwise nothing would watch the
                // container.
                let create = spawn_logged(action, async move {
                    container_log.lock().await.init().await?;

                    let started = child_reaper
                        .create_child(
                            RuntimeCall {
                                stdin,
                                ..RuntimeCall::new(&runtime, &args, &pidfile, runtime_deadline)
                            },
                            &mut container_io,
                        )
                        .await;
                    let StartedChild {
                        pid: grandchild_pid,
                        token,
                        call,
                    } = match started {
                        Ok(started) => started,
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
                        token,
                        process: None,
                    };
                    child_reaper.watch_grandchild(child, call)?;
                    Ok(grandchild_pid)
                });
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
        self: Rc<Self>,
        params: conmon::ExecSyncContainerParams,
        mut results: conmon::ExecSyncContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry!(pry!(req.get_id()).to_string());

        enter_request!(req, "exec_sync_container", id.as_str());

        pry!(reject_retired(
            "execSyncContainer",
            [("envVars", pry!(req.get_env_vars_old()).is_empty())],
        ));
        // Before the PID file and the console socket of a terminal get
        // created, see create_container.
        pry_err!(check_id(&id));

        // The timeout starts now, the runtime may hang before the process
        // even starts.
        let time_to_timeout = exec_deadline(req.get_timeout_sec());

        debug!(
            "Got exec sync container request with timeout {}",
            req.get_timeout_sec()
        );

        let child_reaper = self.reaper().clone();
        let command: Vec<String> = pry!(text_list(req.get_command()));
        let cgroup_manager = pry!(req.get_cgroup_manager());
        let mut exec = pry_err!(ExecStart::new(
            self.config(),
            &id,
            req.get_terminal(),
            &command,
            cgroup_manager,
            "exec_sync",
        ));

        Promise::from_future(
            async move {
                // See create_container, the exec has to be watched and its
                // output drained even if the client goes away.
                // The runtime may hang, it gets killed with a grace on top of
                // the timeout, or after the grace without one. The client gets
                // its timeout right away, the task kills the process the
                // runtime started once it finished.
                let runtime_deadline = ChildReaper::exec_runtime_deadline(time_to_timeout);
                let (started_tx, mut started_rx) = oneshot::channel();
                let exec_task = spawn_logged("run exec", async move {
                    // Read while the runtime runs, see
                    // `ContainerIO::collect_stdio`.
                    exec.collect_stdio(true, true)?;
                    let res = exec.run(&child_reaper, false, runtime_deadline).await;
                    // The client got its timeout response once nothing
                    // receives this anymore, see below.
                    let awaited = started_tx.send(()).is_ok();
                    let started = match res {
                        Ok(started) => started,
                        Err(e) => {
                            let timed_out = time_to_timeout.is_some_and(|t| Instant::now() >= t);
                            let e = exec.runtime_error(e).await;
                            error!("Unable to start exec: {e:#}");
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

                    // A timeout which already passed kills it right away.
                    let watched = exec.watch(&child_reaper, id.into(), started, time_to_timeout)?;
                    // Nobody takes the output of an exec whose client got
                    // its timeout response already, so it does not get
                    // collected. Dropping the exec stops the output once
                    // the process exited, which the watcher kills.
                    if !awaited {
                        debug!("Not collecting the output of the exec, the client got its timeout");
                        return Ok(ExecSyncOutput {
                            stdout: vec![],
                            stderr: vec![],
                            exit_code: EXIT_CODE_TIMED_OUT,
                            timed_out: true,
                        });
                    }
                    let WatchedExec {
                        io,
                        token,
                        process,
                        exit_rx,
                        stop_output: _stop_output,
                    } = watched;
                    exec_sync_output(&io, &token, exit_rx, || {
                        if let Err(e) = process.signal_group(Signal::SIGKILL) {
                            debug!("Not killing exec process {}: {e}", process.pid());
                        }
                    })
                    .await
                });

                let started = select! {
                    _ = &mut started_rx => true,
                    () = sleep_until(time_to_timeout) => false,
                };
                // Tells the task that the client gets its response now, see
                // above. Nothing after this yields to the task before it
                // went out.
                drop(started_rx);
                let output = if started {
                    task_result(exec_task).await?
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
        self: Rc<Self>,
        params: conmon::AttachContainerParams,
        _: conmon::AttachContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry!(pry!(req.get_id()).to_str());

        enter_request!(req, "attach_container", id);

        debug!("Got an attach container request");

        let socket_path = PathBuf::from(pry!(pry!(req.get_socket_path()).to_str()));
        pry_err!(fs_util::check_absolute("socket path", &socket_path));
        let child = pry_err!(self.reaper().get(id));
        let stop_after_stdin_eof = req.get_stop_after_stdin_eof();

        Promise::from_future(
            async move {
                capnp_err!(child.io().attach().add(
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
        self: Rc<Self>,
        params: conmon::ReopenLogContainerParams,
        _: conmon::ReopenLogContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry!(pry!(req.get_id()).to_str());

        enter_request!(req, "reopen_log_container", id);

        debug!("Got a reopen container log request");

        let child = pry_err!(self.reaper().get(id));

        Promise::from_future(
            async move {
                // A dropped call must not stop a reopen halfway, between the
                // write of the old file and the open of the new one.
                let reopen = spawn_logged("reopen log", async move {
                    child.io().logger().lock().await.reopen().await
                });
                task_result(reopen).await
            }
            .instrument(debug_span!("promise")),
        )
    }

    /// Adjust the window size of a container running inside of a terminal.
    fn set_window_size_container(
        self: Rc<Self>,
        params: conmon::SetWindowSizeContainerParams,
        _: conmon::SetWindowSizeContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());
        let id = pry!(pry!(req.get_id()).to_str());

        enter_request!(req, "set_window_size_container", id);

        debug!("Got a set window size container request");

        let child = pry_err!(self.reaper().get(id));
        let width = req.get_width();
        let height = req.get_height();

        Promise::from_future(
            async move { capnp_err!(child.io().resize(width, height)) }
                .instrument(debug_span!("promise")),
        )
    }

    /// Retired, the pause process which created the namespaces is gone.
    fn create_namespaces_old(
        self: Rc<Self>,
        _: conmon::CreateNamespacesOldParams,
        _: conmon::CreateNamespacesOldResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(Error::unimplemented(
            "createNamespaces is not supported by this server".into(),
        ))
    }

    /// Retired, the fd socket is gone.
    fn start_fd_socket_old(
        self: Rc<Self>,
        _: conmon::StartFdSocketOldParams,
        _: conmon::StartFdSocketOldResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(Error::unimplemented(
            "startFdSocket is not supported by this server".into(),
        ))
    }

    fn serve_exec_container(
        self: Rc<Self>,
        params: conmon::ServeExecContainerParams,
        mut results: conmon::ServeExecContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());

        let id = pry!(pry!(req.get_id()).to_string());

        enter_request!(req, "serve_exec_container", id.as_str());

        debug!("Got a serve exec container request");

        // Validate that the container actually exists
        pry_err!(self.reaper().get(&id));

        let command: Vec<String> = pry!(text_list(req.get_command()));
        let (tty, stdin, stdout, stderr) = (
            req.get_tty(),
            req.get_stdin(),
            req.get_stdout(),
            req.get_stderr(),
        );
        let idle_timeout = idle_timeout(req.get_idle_timeout_sec());

        let streaming_server = self.streaming_server().clone();
        let child_reaper = self.reaper().clone();
        let config = self.config().clone();
        let cgroup_manager = pry!(req.get_cgroup_manager());

        Promise::from_future(
            async move {
                let session = ExecSession {
                    child_reaper,
                    tty,
                    server_config: config,
                    cgroup_manager,
                    command,
                    common: SessionCommon {
                        container_id: id.into_boxed_str(),
                        stdin,
                        stdout,
                        stderr,
                        idle_timeout,
                    },
                };
                let url = streaming_result(streaming_server.exec_url(session).await)?;

                results.get().init_response().set_url(&url);
                Ok(())
            }
            .instrument(debug_span!("promise")),
        )
    }

    fn serve_attach_container(
        self: Rc<Self>,
        params: conmon::ServeAttachContainerParams,
        mut results: conmon::ServeAttachContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());

        let id = pry!(pry!(req.get_id()).to_str());

        enter_request!(req, "serve_attach_container", id);

        debug!("Got a serve attach container request");

        let (stdin, stdout, stderr) = (req.get_stdin(), req.get_stdout(), req.get_stderr());
        let stop_after_stdin_eof = req.get_stop_after_stdin_eof();
        let idle_timeout = idle_timeout(req.get_idle_timeout_sec());

        let streaming_server = self.streaming_server().clone();
        let child = pry_err!(self.reaper().get(id));
        let id: Box<str> = id.into();

        Promise::from_future(
            async move {
                let session = AttachSession {
                    child,
                    stop_after_stdin_eof,
                    common: SessionCommon {
                        container_id: id,
                        stdin,
                        stdout,
                        stderr,
                        idle_timeout,
                    },
                };
                let url = streaming_result(streaming_server.attach_url(session).await)?;

                results.get().init_response().set_url(&url);
                Ok(())
            }
            .instrument(debug_span!("promise")),
        )
    }

    fn serve_port_forward_container(
        self: Rc<Self>,
        params: conmon::ServePortForwardContainerParams,
        mut results: conmon::ServePortForwardContainerResults,
    ) -> Promise<(), capnp::Error> {
        let req = pry!(pry!(params.get()).get_request());

        enter_request!(req, "serve_port_forward_container");

        debug!("Got a serve port forward container request");

        // An empty one is not absolute either.
        let netns_path = PathBuf::from(pry!(pry!(req.get_net_ns_path()).to_str()));
        pry_err!(fs_util::check_absolute(
            "network namespace path",
            &netns_path
        ));
        let idle_timeout = idle_timeout(req.get_idle_timeout_sec());
        let streaming_server = self.streaming_server().clone();

        Promise::from_future(
            async move {
                let url = streaming_result(
                    streaming_server
                        .port_forward_url(netns_path, idle_timeout)
                        .await,
                )?;

                results.get().init_response().set_url(&url);
                Ok(())
            }
            .instrument(debug_span!("promise")),
        )
    }
}

#[cfg(test)]
mod tests;
