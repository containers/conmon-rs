use super::*;
use std::{
    ffi::OsStr,
    fs::Permissions,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
};

fn config(args: &[&str]) -> Config {
    Config::parse_for_test(args.iter().copied()).expect("environment checked")
}

#[test]
fn validate_requires_the_runtime_flags() {
    if Config::parse_for_test::<_, &str>([]).is_none() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = dir.path().join("runtime");
    std::fs::write(&runtime, "").expect("write runtime");
    let runtime = runtime.to_str().expect("utf8 path");
    let runtime_dir = dir.path().to_str().expect("utf8 path");

    assert!(config(&[]).validate().is_err());
    assert!(config(&["--runtime", runtime]).validate().is_err());
    assert!(
        config(&["--runtime", "/does/not/exist", "--runtime-dir", runtime_dir])
            .validate()
            .is_err()
    );
    assert!(
        config(&["--runtime", runtime, "--runtime-dir", runtime_dir])
            .validate()
            .is_ok()
    );
    // The runtime root must be a directory if it exists.
    assert!(
        config(&[
            "--runtime",
            runtime,
            "--runtime-dir",
            runtime_dir,
            "--runtime-root",
            runtime
        ])
        .validate()
        .is_err()
    );
}

#[test]
fn paths_get_absolute() -> Result<()> {
    let Some(mut config) = Config::parse_for_test([
        "--runtime",
        "bin/runc",
        "--runtime-dir",
        "run",
        "--runtime-root",
        "/root",
    ]) else {
        return Ok(());
    };
    config.make_absolute()?;
    let cwd = std::env::current_dir()?;
    assert_eq!(config.runtime(), cwd.join("bin/runc"));
    assert_eq!(config.runtime_dir(), cwd.join("run"));
    assert_eq!(config.runtime_root(), Some(Path::new("/root")));
    Ok(())
}

#[test]
fn prepare_keeps_the_sockets_of_a_running_server() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let runtime_dir = dir.path().to_str().context("utf8 path")?;
    let Some(config) = Config::parse_for_test(["--runtime-dir", runtime_dir]) else {
        return Ok(());
    };

    // Left behind by a server which is gone. A process which a parallel
    // test forks may hold it until it execs.
    drop(std::os::unix::net::UnixListener::bind(config.socket())?);
    std::fs::write(config.fd_socket(), "")?;
    let mut res = config.prepare();
    for _ in 0..500 {
        match &res {
            Err(e) if format!("{e:#}").contains("in use") => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                res = config.prepare();
            }
            _ => break,
        }
    }
    res?;
    assert!(!config.socket().exists());
    assert!(!config.fd_socket().exists());

    // A running server keeps them.
    let server = std::os::unix::net::UnixListener::bind(config.socket())?;
    let err = config.prepare().unwrap_err();
    assert!(format!("{err:#}").contains("in use"), "{err:#}");
    assert!(config.socket().exists());
    drop(server);
    std::fs::remove_file(config.socket())?;

    // A socket of another type is no server of this kind, one of another
    // user is not reachable either.
    // Root can connect anyway.
    let root = nix::unistd::geteuid().is_root();
    let _other_type = std::os::unix::net::UnixListener::bind(config.fd_socket())?;
    let _inaccessible = if root {
        None
    } else {
        let listener = std::os::unix::net::UnixListener::bind(config.socket())?;
        std::fs::set_permissions(config.socket(), Permissions::from_mode(0o000))?;
        Some(listener)
    };
    config.prepare()?;
    assert!(!config.fd_socket().exists());
    assert!(!config.socket().exists());
    Ok(())
}

#[test]
fn validate_reports_each_check() {
    let Some(base) = Config::parse_for_test::<_, &str>([]) else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = dir.path().join("runtime");
    std::fs::write(&runtime, "").expect("write runtime");
    let error = |config: &Config| {
        config
            .validate()
            .map(|()| String::new())
            .unwrap_or_else(|e| format!("{e:#}"))
    };

    let valid = Config {
        runtime: runtime.clone(),
        runtime_dir: dir.path().into(),
        ..base
    };
    assert_eq!(error(&valid), "");

    // Blanks count as unset, that is the default of both flags.
    let blank_runtime = Config {
        runtime: "  ".into(),
        ..valid.clone()
    };
    assert_eq!(error(&blank_runtime), "--runtime flag not set");
    let blank_runtime_dir = Config {
        runtime_dir: " ".into(),
        ..valid.clone()
    };
    assert_eq!(error(&blank_runtime_dir), "--runtime-dir flag not set");

    let not_utf8 = Config {
        runtime: OsStr::from_bytes(b"/bin/\xff").into(),
        ..valid.clone()
    };
    assert_eq!(error(&not_utf8), "--runtime does not parse as string");

    let missing_runtime = Config {
        runtime: dir.path().join("missing"),
        ..valid.clone()
    };
    assert_eq!(
        error(&missing_runtime),
        format!(
            "--runtime '{}' does not exist",
            dir.path().join("missing").display()
        )
    );

    // A runtime root which does not exist yet gets created by prepare.
    let missing_root = Config {
        runtime_root: Some(dir.path().join("root")),
        ..valid.clone()
    };
    assert_eq!(error(&missing_root), "");
    let dir_root = Config {
        runtime_root: Some(dir.path().into()),
        ..valid.clone()
    };
    assert_eq!(error(&dir_root), "");
    let file_root = Config {
        runtime_root: Some(runtime.clone()),
        ..valid
    };
    assert_eq!(
        error(&file_root),
        format!("--runtime-root '{}' is not a directory", runtime.display())
    );
}
