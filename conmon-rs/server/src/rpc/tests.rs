use super::*;
use crate::{
    child_reaper::ReapableChild, config::Config, conmon_capnp::conmon::CgroupManager,
    streaming_server::DEFAULT_CAPACITY,
};
use std::{process::Stdio, sync::Arc};
use tokio::{process::Command, task::LocalSet, time};

#[tokio::test]
async fn too_many_streaming_sessions_fail_with_the_bare_message() -> anyhow::Result<()> {
    let config = Arc::new(Config::parse_for_test::<_, &str>([]));
    let (_, child, _) = ReapableChild::from_child(Child {
        id: "id".into(),
        kind: ChildKind::Container,
        pid: Pid::from_raw(0),
        exit_paths: vec![],
        oom_exit_paths: vec![],
        timeout: None,
        io: SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?),
        cleanup_cmd: vec![],
        token: CancellationToken::new(),
    });
    let exec = || {
        StreamingRequest::Exec(ExecSession {
            child_reaper: Arc::default(),
            tty: false,
            server_config: config.clone(),
            cgroup_manager: CgroupManager::Cgroupfs,
            container_id: "id".into(),
            command: vec!["true".into()],
            stdin: false,
            stdout: true,
            stderr: false,
            idle_timeout: None,
        })
    };
    let attach = || {
        StreamingRequest::Attach(AttachSession {
            container_id: "id".into(),
            child: child.clone(),
            stdin: false,
            stdout: true,
            stderr: false,
            stop_after_stdin_eof: false,
            idle_timeout: None,
        })
    };
    let port_forward = || StreamingRequest::PortForward {
        netns_path: PathBuf::new(),
        idle_timeout: None,
    };

    LocalSet::new()
        .run_until(async {
            let streaming_server = StreamingServer::default();
            // Exec and attach sessions count together.
            for i in 0..DEFAULT_CAPACITY {
                let request = if i % 2 == 0 { exec() } else { attach() };
                Server::streaming_url(&streaming_server, request).await?;
                Server::streaming_url(&streaming_server, port_forward()).await?;
            }
            for (kind, request) in [
                ("exec", exec()),
                ("attach", attach()),
                ("port forward", port_forward()),
            ] {
                let err = Server::streaming_url(&streaming_server, request)
                    .await
                    .expect_err(kind);
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

/// The message of the error which a request fails with.
async fn failure<T>(response: impl Future<Output = capnp::Result<T>>) -> String {
    match response.await {
        Ok(_) => panic!("request succeeded"),
        Err(e) => {
            assert_eq!(e.kind, capnp::ErrorKind::Failed, "{e}");
            e.extra
        }
    }
}

#[tokio::test]
async fn requests_get_validated() -> anyhow::Result<()> {
    let config = Config::parse_for_test::<_, &str>([]);
    // A local capability, the requests need no socket.
    let client: conmon::Client = capnp_rpc::new_client(Server::with_config(config));

    let create = |id: &str, bundle: &str, exit_path: &str| {
        let mut request = client.create_container_request();
        let mut req = request.get().init_request();
        req.set_id(id);
        req.set_bundle_path(bundle);
        req.init_exit_paths(1).set(0, exit_path);
        request.send().promise
    };
    assert_eq!(
        failure(create("id", "bundle", "/exit")).await,
        r#"bundle path "bundle" is not absolute"#
    );
    assert_eq!(
        failure(create("id", "/bundle", "exit")).await,
        r#"exit path "exit" is not absolute"#
    );
    assert_eq!(
        failure(create("--help", "/bundle", "/exit")).await,
        r#"invalid container ID "--help""#
    );

    let mut request = client.create_container_request();
    let mut req = request.get().init_request();
    req.set_id("id");
    req.set_bundle_path("/bundle");
    req.set_sdnotify_socket("notify.sock");
    assert_eq!(
        failure(request.send().promise).await,
        r#"sdnotify socket "notify.sock" is not absolute"#
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

#[test]
fn exec_deadline() {
    assert!(Server::exec_deadline(0).is_none());
    // Too far in the future to represent, which is no timeout either.
    assert!(Server::exec_deadline(u64::MAX).is_none());
    let deadline = Server::exec_deadline(10).expect("deadline");
    assert!(deadline > Instant::now() + Duration::from_secs(9));
}

#[tokio::test(start_paused = true)]
async fn exec_runtimes_always_get_a_deadline() {
    let now = Instant::now();
    let timeout = now + Duration::from_secs(10);
    assert_eq!(
        Server::exec_runtime_deadline(Some(timeout)),
        timeout + Server::EXEC_RUNTIME_GRACE
    );
    // Without a timeout of the process, the runtime still gets one.
    assert_eq!(
        Server::exec_runtime_deadline(None),
        now + Server::EXEC_RUNTIME_GRACE
    );
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
        let (output, watched) = tokio::join!(
            Server::exec_sync_output(&io, &token, exit_rx, || {}),
            watcher
        );
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
        Server::exec_sync_output(&io, &token, exit_rx, || killed = child.start_kill().is_ok()),
    )
    .await?;
    assert!(res.is_err());
    assert!(killed);
    assert!(token.is_cancelled());
    time::timeout(Duration::from_secs(10), child.wait()).await??;
    Ok(())
}
