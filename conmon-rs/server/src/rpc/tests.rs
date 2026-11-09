use super::*;
use crate::{
    child_reaper::ReapableChild,
    config::Config,
    conmon_capnp::conmon::CgroupManager,
    process::Process,
    streaming_server::{DEFAULT_CAPACITY, StreamingServer},
    test_util::{Spawned, container_child, write_script},
};
use nix::unistd::Pid;
use std::{fs, process::Stdio, sync::Arc};
use tokio::{process::Command, task::LocalSet, time};

#[tokio::test]
async fn too_many_streaming_sessions_fail_with_the_bare_message() -> anyhow::Result<()> {
    let config = Arc::new(Config::parse_for_test::<_, &str>([]));
    let (_, child, _) = ReapableChild::from_child(container_child(false)?);
    let streaming_server = StreamingServer::default();
    let common = || SessionCommon {
        container_id: "id".into(),
        stdin: false,
        stdout: true,
        stderr: false,
        idle_timeout: None,
    };
    let exec = || {
        streaming_server.exec_url(ExecSession {
            child_reaper: Arc::default(),
            tty: false,
            server_config: config.clone(),
            cgroup_manager: CgroupManager::Cgroupfs,
            command: vec!["true".into()],
            common: common(),
        })
    };
    let attach = || {
        streaming_server.attach_url(AttachSession {
            child: child.clone(),
            stop_after_stdin_eof: false,
            common: common(),
        })
    };
    let port_forward = || streaming_server.port_forward_url(PathBuf::new(), None);

    LocalSet::new()
        .run_until(async {
            // Exec and attach sessions count together.
            for i in 0..DEFAULT_CAPACITY {
                if i % 2 == 0 {
                    streaming_result(exec().await)?;
                } else {
                    streaming_result(attach().await)?;
                }
                streaming_result(port_forward().await)?;
            }
            for (kind, res) in [
                ("exec", exec().await),
                ("attach", attach().await),
                ("port forward", port_forward().await),
            ] {
                let err = streaming_result(res).expect_err(kind);
                // The message alone reaches the client, the start of it
                // matches `ErrTooManyRequests` of the Go client.
                assert_eq!(err.kind, capnp::ErrorKind::Failed, "{kind}");
                assert_eq!(
                    err.extra, "maximum number of in-flight requests exceeded",
                    "{kind}"
                );
            }
            Ok(())
        })
        .await
}

/// The message of the error of the `kind` which a request fails with.
async fn failure_of_kind<T>(
    kind: capnp::ErrorKind,
    response: impl Future<Output = capnp::Result<T>>,
) -> String {
    match response.await {
        Ok(_) => panic!("request succeeded"),
        Err(e) => {
            assert_eq!(e.kind, kind, "{e}");
            e.extra
        }
    }
}

/// The message of the error which a request fails with.
async fn failure<T>(response: impl Future<Output = capnp::Result<T>>) -> String {
    failure_of_kind(capnp::ErrorKind::Failed, response).await
}

/// The retired requests fail with a message which names them, for the
/// older clients.
#[tokio::test]
async fn retired_requests_are_not_supported() {
    let config = Config::parse_for_test::<_, &str>([]);
    let client: conmon::Client = capnp_rpc::new_client(Server::with_config(config));
    let unimplemented = capnp::ErrorKind::Unimplemented;

    let request = client.create_namespaces_old_request();
    assert_eq!(
        failure_of_kind(unimplemented, request.send().promise).await,
        "createNamespaces is not supported by this server"
    );
    let request = client.start_fd_socket_old_request();
    assert_eq!(
        failure_of_kind(unimplemented, request.send().promise).await,
        "startFdSocket is not supported by this server"
    );

    // A container which relies on a retired field does not get created
    // without it.
    type Set = dyn Fn(conmon::create_container_request::Builder<'_>);
    let create = |set: &Set| {
        let mut request = client.create_container_request();
        let mut req = request.get().init_request();
        req.set_id("id");
        req.set_bundle_path("/bundle");
        set(req);
        request.send().promise
    };
    let retired: [(&str, &Set); 9] = [
        ("sdnotifySocket", &|mut req| {
            req.set_sdnotify_socket_old("/run/systemd/notify");
        }),
        ("cleanupCmd", &|req| {
            req.init_cleanup_cmd_old(1).set(0, "true");
        }),
        ("globalArgs", &|req| {
            req.init_global_args_old(1).set(0, "--root=/run/other");
        }),
        ("envVars", &|req| {
            let mut entry = req.init_env_vars_old(1).get(0);
            entry.set_key("KEY");
            entry.set_value("value");
        }),
        ("additionalFds", &|req| {
            req.init_additional_fds_old(1).set(0, 1);
        }),
        ("leakFds", &|req| {
            req.init_leak_fds_old(1).set(0, 1);
        }),
        ("json log driver", &|req| {
            let mut driver = req.init_log_drivers(1).get(0);
            driver.set_type(conmon::log_driver::Type::JsonOld);
            driver.set_path("/log");
        }),
        ("tag of a log driver", &|req| {
            let mut driver = req.init_log_drivers(1).get(0);
            driver.set_type(conmon::log_driver::Type::Journald);
            driver.set_tag_old("tag");
        }),
        ("labels of a log driver", &|req| {
            let mut driver = req.init_log_drivers(1).get(0);
            driver.set_type(conmon::log_driver::Type::Journald);
            let mut label = driver.init_labels_old(1).get(0);
            label.set_key("KEY");
            label.set_value("value");
        }),
    ];
    for (field, set) in retired {
        assert_eq!(
            failure_of_kind(unimplemented, create(set)).await,
            format!("the {field} of createContainer is not supported by this server")
        );
    }

    let mut request = client.exec_sync_container_request();
    let mut req = request.get().init_request();
    req.set_id("id");
    req.reborrow().init_command(1).set(0, "true");
    let mut entry = req.init_env_vars_old(1).get(0);
    entry.set_key("KEY");
    entry.set_value("value");
    assert_eq!(
        failure_of_kind(unimplemented, request.send().promise).await,
        "the envVars of execSyncContainer is not supported by this server"
    );
}

/// The older clients send the retired fields empty, which passes.
#[tokio::test]
async fn empty_retired_fields_pass() {
    let config = Config::parse_for_test::<_, &str>([]);
    let client: conmon::Client = capnp_rpc::new_client(Server::with_config(config));
    let mut request = client.create_container_request();
    let mut req = request.get().init_request();
    req.set_id("id");
    // Fails after the retired fields got checked.
    req.set_bundle_path("bundle");
    req.reborrow().init_cleanup_cmd_old(0);
    req.reborrow().init_global_args_old(0);
    req.reborrow().init_env_vars_old(0);
    req.reborrow().init_additional_fds_old(0);
    req.reborrow().init_leak_fds_old(0);
    req.set_sdnotify_socket_old("");
    let mut driver = req.init_log_drivers(1).get(0);
    driver.set_type(conmon::log_driver::Type::Journald);
    driver.set_tag_old("");
    driver.init_labels_old(0);
    assert_eq!(
        failure(request.send().promise).await,
        r#"bundle path "bundle" is not absolute"#
    );

    let mut request = client.exec_sync_container_request();
    let mut req = request.get().init_request();
    // Fails after the retired fields got checked.
    req.set_id("--help");
    req.reborrow().init_command(1).set(0, "true");
    req.init_env_vars_old(0);
    assert_eq!(
        failure(request.send().promise).await,
        r#"invalid container ID "--help""#
    );
}

#[tokio::test]
async fn requests_get_validated() -> anyhow::Result<()> {
    let config = Config::parse_for_test::<_, &str>([]);
    // A local capability, the requests need no socket.
    let client: conmon::Client = capnp_rpc::new_client(Server::with_config(config));

    let create = |id: &str, bundle: &str, exit_path: &str, oom_exit_path: &str| {
        let mut request = client.create_container_request();
        let mut req = request.get().init_request();
        req.set_id(id);
        req.set_bundle_path(bundle);
        req.reborrow().init_exit_paths(1).set(0, exit_path);
        req.init_oom_exit_paths(1).set(0, oom_exit_path);
        request.send().promise
    };
    assert_eq!(
        failure(create("id", "bundle", "/exit", "/oom")).await,
        r#"bundle path "bundle" is not absolute"#
    );
    assert_eq!(
        failure(create("id", "/bundle", "exit", "/oom")).await,
        r#"exit path "exit" is not absolute"#
    );
    assert_eq!(
        failure(create("id", "/bundle", "/exit", "oom")).await,
        r#"OOM exit path "oom" is not absolute"#
    );
    assert_eq!(
        failure(create("--help", "/bundle", "/exit", "/oom")).await,
        r#"invalid container ID "--help""#
    );
    // The request gets checked before anything gets set up for it, the ID
    // first, then the paths in the order above.
    assert_eq!(
        failure(create("--help", "bundle", "exit", "oom")).await,
        r#"invalid container ID "--help""#
    );
    assert_eq!(
        failure(create("id", "bundle", "exit", "oom")).await,
        r#"bundle path "bundle" is not absolute"#
    );
    assert_eq!(
        failure(create("id", "/bundle", "exit", "oom")).await,
        r#"exit path "exit" is not absolute"#
    );

    // The checkpoint of a restore has to be absolute like the other paths,
    // an empty one is a create.
    let mut request = client.create_container_request();
    let mut req = request.get().init_request();
    req.set_id("id");
    req.set_bundle_path("/bundle");
    req.set_restore_path("checkpoint");
    assert_eq!(
        failure(request.send().promise).await,
        r#"restore path "checkpoint" is not absolute"#
    );

    let mut request = client.attach_container_request();
    let mut req = request.get().init_request();
    req.set_id("id");
    req.set_socket_path("attach");
    assert_eq!(
        failure(request.send().promise).await,
        r#"socket path "attach" is not absolute"#
    );

    for path in ["netns", ""] {
        let mut request = client.serve_port_forward_container_request();
        request.get().init_request().set_net_ns_path(path);
        assert_eq!(
            failure(request.send().promise).await,
            format!("network namespace path {path:?} is not absolute")
        );
    }

    // Clients match the start, see `ErrContainerNotFound` of the Go client.
    let mut request = client.reopen_log_container_request();
    request.get().init_request().set_id("unknown");
    let err = failure(request.send().promise).await;
    assert!(err.starts_with("container not found"), "{err}");

    // The Go client resizes the terminal of an attach with it.
    let mut request = client.set_window_size_container_request();
    let mut req = request.get().init_request();
    req.set_id("unknown");
    req.set_width(80);
    req.set_height(24);
    let err = failure(request.send().promise).await;
    assert!(err.starts_with("container not found"), "{err}");
    Ok(())
}

/// Run an exec through the RPC with a runtime which runs `script`, with
/// a timeout of a second.
async fn exec_sync_with_runtime(script: &str) -> anyhow::Result<ExecSyncOutput> {
    let dir = tempfile::tempdir()?;
    let runtime = dir.path().join("runtime");
    crate::test_util::write_script(&runtime, script)?;
    let config = Config::parse_for_test([
        "--runtime",
        runtime.to_str().context("runtime")?,
        "--runtime-dir",
        dir.path().to_str().context("runtime dir")?,
    ]);

    let client: conmon::Client = capnp_rpc::new_client(Server::with_config(config));
    let mut request = client.exec_sync_container_request();
    let mut req = request.get().init_request();
    req.set_id("ctr");
    req.set_timeout_sec(1);
    req.init_command(1).set(0, "true");
    let reply = request.send().promise.await?;
    let resp = reply.get()?.get_response()?;
    Ok(ExecSyncOutput {
        stdout: resp.get_stdout()?.to_vec(),
        stderr: resp.get_stderr()?.to_vec(),
        exit_code: resp.get_exit_code(),
        timed_out: resp.get_timed_out(),
    })
}

#[tokio::test]
async fn exec_sync_reports_runtimes_which_do_not_start_the_process() -> anyhow::Result<()> {
    LocalSet::new()
        .run_until(async {
            // The runtime fails, its stderr tells why.
            let failed = exec_sync_with_runtime("echo boom >&2; exit 1").await?;
            assert_eq!(failed.exit_code, EXIT_CODE_NOT_STARTED);
            assert!(!failed.timed_out);
            let stderr = String::from_utf8_lossy(&failed.stderr);
            assert!(stderr.contains("boom"), "{stderr}");

            // The runtime hangs beyond the timeout of the exec.
            let hung = exec_sync_with_runtime("exec sleep 60").await?;
            assert_eq!(hung.exit_code, EXIT_CODE_TIMED_OUT);
            assert!(hung.timed_out);
            let stderr = String::from_utf8_lossy(&hung.stderr);
            assert!(
                stderr.contains("timed out waiting for the runtime"),
                "{stderr}"
            );
            Ok(())
        })
        .await
}

/// A bundle with a runtime of its own, for the creates through the RPC, see
/// [`Bundle::with_runtime`].
struct Bundle {
    dir: tempfile::TempDir,
    client: conmon::Client,
}

impl Bundle {
    /// A bundle whose runtime records the arguments it got in the file
    /// `args` of the bundle, one per line, takes the `--pid-file` argument
    /// into `$PIDFILE` and runs `script` then.
    fn with_runtime(script: &str) -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let runtime = dir.path().join("runtime");
        let prelude = format!(
            "printf '%s\\n' \"$@\" > {}\nwhile [ $# -gt 0 ] && [ \"$1\" != --pid-file ]; do shift; done\nPIDFILE=$2",
            dir.path().join("args").display()
        );
        write_script(&runtime, &format!("{prelude}\n{script}"))?;
        let config = Config::parse_for_test([
            "--runtime",
            runtime.to_str().context("runtime")?,
            "--runtime-dir",
            dir.path().to_str().context("runtime dir")?,
        ]);
        let client = capnp_rpc::new_client(Server::with_config(config));
        Ok(Self { dir, client })
    }

    fn path(&self) -> String {
        self.dir.path().display().to_string()
    }

    /// The PID file which the runtime gets.
    fn pidfile(&self) -> PathBuf {
        self.dir.path().join("pidfile")
    }

    /// The arguments the runtime got.
    fn args(&self) -> anyhow::Result<Vec<String>> {
        let args = fs::read_to_string(self.dir.path().join("args"))?;
        Ok(args.lines().map(Into::into).collect())
    }

    /// Create the container `id` in the bundle with two command args, or
    /// restore it from the `restore_path` if that is not empty, and return
    /// the PID of the response.
    fn create(&self, id: &str, restore_path: &str) -> impl Future<Output = capnp::Result<Pid>> {
        let mut request = self.client.create_container_request();
        let mut req = request.get().init_request();
        req.set_id(id);
        req.set_bundle_path(self.path());
        req.set_restore_path(restore_path);
        req.set_cgroup_manager(CgroupManager::Cgroupfs);
        let mut command_args = req.init_command_args(2);
        command_args.set(0, "--no-pivot");
        command_args.set(1, "--opt=1");
        let promise = request.send().promise;
        async move {
            let reply = promise.await?;
            let pid = reply.get()?.get_response()?.get_container_pid();
            let pid = i32::try_from(pid).map_err(|e| Error::failed(e.to_string()))?;
            Ok(Pid::from_raw(pid))
        }
    }

    /// Reopen the log of the container `id`, which fails for one the server
    /// does not know.
    async fn reopen_log(&self, id: &str) -> capnp::Result<()> {
        let mut request = self.client.reopen_log_container_request();
        request.get().init_request().set_id(id);
        request.send().promise.await?;
        Ok(())
    }
}

#[tokio::test]
async fn create_runs_the_runtime_and_registers_the_container() -> anyhow::Result<()> {
    LocalSet::new()
        .run_until(async {
            // The process which the runtime starts outlives it, like a
            // container process which got reparented to the server.
            let bundle = Bundle::with_runtime("sleep 60 & echo $! > \"$PIDFILE\"")?;
            let dir = bundle.path();
            let pidfile = bundle.pidfile().display().to_string();
            let create = ["create", "--bundle", &dir, "--pid-file", &pidfile];
            let restore = [
                "restore",
                "--bundle",
                &dir,
                "--pid-file",
                &pidfile,
                "--detach",
                "--image-path",
                "/ckpt",
                "--work-path",
                &dir,
            ];
            for (id, restore_path, command) in [
                ("created", "", &create[..]),
                ("restored", "/ckpt", &restore[..]),
            ] {
                let pid = bundle.create(id, restore_path).await?;
                // Kills the process once done with it.
                let _process = Spawned {
                    pid,
                    process: Process::open(pid),
                };

                // The command args follow the options of the server, the ID
                // comes last.
                let mut expected: Vec<String> = command.iter().map(|arg| arg.to_string()).collect();
                expected.extend(["--no-pivot".into(), "--opt=1".into(), id.into()]);
                assert_eq!(bundle.args()?, expected, "{id}");

                // The response carries the PID the runtime wrote.
                let written = fs::read_to_string(bundle.pidfile())?
                    .trim()
                    .parse::<i32>()?;
                assert_eq!(pid.as_raw(), written, "{id}");

                // The server knows the container, its log can be reopened.
                bundle.reopen_log(id).await?;
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn failed_create_reports_the_runtime_and_registers_nothing() -> anyhow::Result<()> {
    LocalSet::new()
        .run_until(async {
            let bundle = Bundle::with_runtime("echo boom >&2; exit 1")?;
            let err = failure(bundle.create("ctr", "")).await;
            assert!(err.starts_with("runtime exited with status 1"), "{err}");
            // The stderr of the runtime tells why.
            assert!(err.contains("boom"), "{err}");

            // Nothing got registered under the ID.
            let err = failure(bundle.reopen_log("ctr")).await;
            assert!(err.starts_with("container not found"), "{err}");
            Ok(())
        })
        .await
}

#[test]
fn exec_deadlines() {
    assert!(exec_deadline(0).is_none());
    // Too far in the future to represent, which is no timeout either.
    assert!(exec_deadline(u64::MAX).is_none());
    let deadline = exec_deadline(10).expect("deadline");
    assert!(deadline > Instant::now() + Duration::from_secs(9));
}

/// The IO of an exec which runs `script`, its process and the token which
/// its watcher cancels.
fn exec_io(
    script: &str,
) -> anyhow::Result<(SharedContainerIO, tokio::process::Child, CancellationToken)> {
    let mut io = ContainerIO::new(false, ContainerLog::new())?;
    let mut child = Command::new("sh")
        .args(["-c", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let token = CancellationToken::new();
    io.connect_streams(
        None,
        child.stdout.take(),
        child.stderr.take(),
        token.clone(),
    );
    Ok((SharedContainerIO::new(io), child, token))
}

#[tokio::test]
async fn exec_sync_output_takes_the_result_from_the_watcher() -> anyhow::Result<()> {
    // The process exits right at its timeout, after its last output. Only
    // the watcher knows whether that was in time, the output is complete
    // either way.
    for timed_out in [false, true] {
        let (io, mut child, token) = exec_io("echo last")?;
        let (exit_tx, exit_rx) = broadcast::channel(1);
        let watcher = async {
            child.wait().await?;
            // Output still in flight gets picked up by the drain after the
            // cancel.
            token.cancel();
            exit_tx.send(ExitChannelData {
                exit_code: if timed_out { EXIT_CODE_TIMED_OUT } else { 0 },
                timed_out,
            })?;
            anyhow::Ok(())
        };
        let (output, watched) =
            tokio::join!(exec_sync_output(&io, &token, exit_rx, || {}), watcher);
        watched?;
        let output = output?;
        assert_eq!(output.stdout, b"last\n");
        assert_eq!(output.timed_out, timed_out);
        assert_eq!(output.exit_code == EXIT_CODE_TIMED_OUT, timed_out);
    }
    Ok(())
}

#[tokio::test]
async fn exec_sync_output_kills_the_process_of_a_lost_watcher() -> anyhow::Result<()> {
    let (io, mut child, token) = exec_io("echo started; exec sleep 600")?;
    let (exit_tx, exit_rx) = broadcast::channel::<ExitChannelData>(1);
    // Nothing ever cancels the token, the process keeps the pipes open.
    drop(exit_tx);
    let mut killed = false;
    let res = time::timeout(
        Duration::from_secs(10),
        exec_sync_output(&io, &token, exit_rx, || killed = child.start_kill().is_ok()),
    )
    .await?;
    let err = res.err().map(|e| format!("{e:#}"));
    assert_eq!(
        err.as_deref(),
        Some("receive the exit of the exec: channel closed")
    );
    assert!(killed);
    assert!(token.is_cancelled());
    time::timeout(Duration::from_secs(10), child.wait()).await??;
    Ok(())
}

#[test]
fn exec_sync_output_is_capped_in_total() {
    let max = ExecSyncOutput::MAX_SIZE;
    let half = max / 2;
    for (stdout, stderr, expected) in [
        (10, 20, (10, 20)),
        (max - 10, 20, (max - 20, 20)),
        (20, max + 10, (20, max - 20)),
        (max + 10, max + 10, (half, max - half)),
        (half + 10, max, (half, max - half)),
        (0, max + 10, (0, max)),
    ] {
        let (mut stdout, mut stderr) = (vec![1; stdout], vec![2; stderr]);
        ExecSyncOutput::cap(&mut stdout, &mut stderr);
        assert_eq!((stdout.len(), stderr.len()), expected);
    }
}

#[tokio::test]
async fn exec_sync_output_of_both_pipes_fits_a_response() -> anyhow::Result<()> {
    // More than the limit on each pipe.
    let (io, mut child, token) =
        exec_io("head -c 17000000 /dev/zero; head -c 17000000 /dev/zero >&2")?;
    let (exit_tx, exit_rx) = broadcast::channel(1);
    let watcher = async {
        child.wait().await?;
        token.cancel();
        exit_tx.send(ExitChannelData {
            exit_code: 0,
            timed_out: false,
        })?;
        anyhow::Ok(())
    };
    let (output, watched) = tokio::join!(exec_sync_output(&io, &token, exit_rx, || {}), watcher);
    watched?;
    let output = output?;
    let half = ExecSyncOutput::MAX_SIZE / 2;
    assert_eq!(output.stdout.len(), half);
    assert_eq!(output.stderr.len(), ExecSyncOutput::MAX_SIZE - half);
    Ok(())
}
