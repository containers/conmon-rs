use super::*;
use crate::container_log::ContainerLog;

/// Build the arguments with a configuration from `flags`, None if the
/// environment would change it.
fn runtime_args<F>(flags: &[&str], cgroup_manager: CgroupManager, f: F) -> Option<Vec<String>>
where
    F: FnOnce(GenerateRuntimeArgs) -> Result<Vec<String>>,
{
    runtime_args_with_io(flags, cgroup_manager, false, f)
}

fn runtime_args_with_io<F>(
    flags: &[&str],
    cgroup_manager: CgroupManager,
    terminal: bool,
    f: F,
) -> Option<Vec<String>>
where
    F: FnOnce(GenerateRuntimeArgs) -> Result<Vec<String>>,
{
    let config = Config::parse_for_test(flags.iter().copied())?;
    let container_io =
        ContainerIO::new(terminal, ContainerLog::new()).expect("create container IO");
    let args = GenerateRuntimeArgs {
        config: &config,
        id: "id",
        container_io: &container_io,
        pidfile: Path::new("/pidfile"),
        cgroup_manager,
    };
    Some(f(args).expect("generate runtime args"))
}

fn create(args: GenerateRuntimeArgs) -> Result<Vec<String>> {
    args.create_args(Path::new("/bundle"), ["--global"], ["--command"])
}

#[test]
fn create_args_for_runc() {
    let Some(args) = runtime_args(
        &["--runtime", "/usr/bin/runc"],
        CgroupManager::Cgroupfs,
        create,
    ) else {
        return;
    };
    assert_eq!(
        args,
        [
            "--global",
            "create",
            "--bundle",
            "/bundle",
            "--pid-file",
            "/pidfile",
            "--command",
            "id"
        ]
    );
}

#[test]
fn ids_which_look_like_options_are_rejected() {
    let Some(config) = Config::parse_for_test(["--runtime", "/usr/bin/runc"]) else {
        return;
    };
    let container_io = ContainerIO::new(false, ContainerLog::new()).expect("create container IO");
    for id in ["", "-d", "--help"] {
        let args = GenerateRuntimeArgs {
            config: &config,
            id,
            container_io: &container_io,
            pidfile: Path::new("/pidfile"),
            cgroup_manager: CgroupManager::Cgroupfs,
        };
        assert!(args.exec_sync_args(["true"]).is_err(), "{id}");
        assert!(create(args).is_err(), "{id}");
    }
}

#[test]
fn create_args_for_crun_with_root_and_systemd() {
    let Some(args) = runtime_args(
        &[
            "--runtime",
            "/usr/bin/crun",
            "--runtime-root",
            "/run/crun",
            "--log-level",
            "debug",
        ],
        CgroupManager::Systemd,
        create,
    ) else {
        return;
    };
    assert_eq!(
        args,
        [
            "--log=journald:id",
            "--log-level=debug",
            "--root=/run/crun",
            "--systemd-cgroup",
            "--global",
            "create",
            "--bundle",
            "/bundle",
            "--pid-file",
            "/pidfile",
            "--command",
            "id"
        ]
    );
}

#[test]
fn default_args_crun_log_levels() {
    // crun takes debug, warning and error, the other levels keep its
    // default.
    for (level, expected) in [
        ("trace", None),
        ("debug", Some("--log-level=debug")),
        ("info", None),
        ("warn", Some("--log-level=warning")),
        ("error", Some("--log-level=error")),
        ("off", None),
    ] {
        let Some(args) = runtime_args(
            &["--runtime", "/usr/bin/crun", "--log-level", level],
            CgroupManager::Cgroupfs,
            |args| args.default_args(),
        ) else {
            return;
        };
        let mut want = vec!["--log=journald:id"];
        want.extend(expected);
        assert_eq!(args, want, "level {level}");
    }

    // Other runtimes get neither the journald log nor the log level.
    let Some(args) = runtime_args(
        &["--runtime", "/usr/bin/runc", "--log-level", "debug"],
        CgroupManager::Cgroupfs,
        |args| args.default_args(),
    ) else {
        return;
    };
    assert!(args.is_empty());
}

#[test]
fn exec_sync_args_without_terminal() {
    let Some(args) = runtime_args(
        &["--runtime", "/usr/bin/runc"],
        CgroupManager::Systemd,
        |args| args.exec_sync_args(["sh", "-c", "true"]),
    ) else {
        return;
    };
    assert_eq!(
        args,
        [
            "--systemd-cgroup",
            "exec",
            "-d",
            "--pid-file=/pidfile",
            "id",
            "sh",
            "-c",
            "true"
        ]
    );
}

#[tokio::test]
async fn exec_and_create_args_with_terminal() {
    let mut socket = None;
    let Some(exec) = runtime_args_with_io(
        &["--runtime", "/usr/bin/runc"],
        CgroupManager::Cgroupfs,
        true,
        |args| {
            if let ContainerIOType::Terminal(terminal) = args.container_io.typ() {
                socket = Some(terminal.path().display().to_string());
            }
            args.exec_sync_args(["true"])
        },
    ) else {
        return;
    };
    let console_socket = format!("--console-socket={}", socket.expect("terminal socket"));
    assert_eq!(
        exec,
        [
            "exec",
            "-d",
            console_socket.as_str(),
            "--tty",
            "--pid-file=/pidfile",
            "id",
            "true"
        ]
    );

    let mut socket = None;
    let Some(create) = runtime_args_with_io(
        &["--runtime", "/usr/bin/runc"],
        CgroupManager::Cgroupfs,
        true,
        |args| {
            if let ContainerIOType::Terminal(terminal) = args.container_io.typ() {
                socket = Some(terminal.path().display().to_string());
            }
            create(args)
        },
    ) else {
        return;
    };
    let console_socket = format!("--console-socket={}", socket.expect("terminal socket"));
    // The console socket follows the command args, the ID comes last.
    assert_eq!(
        create,
        [
            "--global",
            "create",
            "--bundle",
            "/bundle",
            "--pid-file",
            "/pidfile",
            "--command",
            console_socket.as_str(),
            "id"
        ]
    );
}
