use anyhow::{Context, Result};
use nix::{
    sys::{
        signal::{kill, SIGINT},
        wait,
    },
    unistd::Pid,
};
use std::fs::read_to_string;
use std::{env, path::PathBuf};
use tempfile::NamedTempFile;
use tokio::{
    fs,
    process::Command,
    time::{sleep, Duration},
};

// The maximum amount of allowed VmRSS in Kilobytes
const MAX_RSS_KB: u32 = 3200;

// We assume that the tests run in release mode
const SERVER_BINARY: &str = "conmon-server";
const CLIENT_BINARY: &str = "conmon-client";

#[tokio::test]
async fn rss_verification() -> Result<()> {
    let pidfile = NamedTempFile::new()?;
    let pidfile_arg = format!("--conmon-pidfile={}", pidfile.path().display());
    // Start the server
    let mut server = Command::new(server_binary())
        .arg("--runtime=/tmp")
        .arg(pidfile_arg)
        .spawn()?;

    // Wait until parent has terminated.
    server.wait().await?;

    // However, that's not our actual server, that's the parent.
    // We need to wait for the child now.
    let pid = read_to_string(pidfile)?.parse::<i32>()?;
    tokio::spawn(async move {
        wait::waitpid(Pid::from_raw(pid), None).expect("wait for server process");
    });

    // Wait for the server up and running
    for i in 1..101 {
        let status = Command::new(client_binary()).status().await?;
        if status.success() {
            break;
        }
        assert!(i != 100);
        sleep(Duration::from_millis(200)).await;
    }

    // Retrieve the RSS
    let contents = fs::read_to_string(format!("/proc/{}/status", pid)).await?;
    let mut rss_res = "";
    for line in contents.lines() {
        if line.starts_with("VmRSS:") {
            rss_res = line
                .split_whitespace()
                .nth(1)
                .context("split by whitespace")?;
            break;
        }
    }
    Command::new("pmap")
        .arg("-x")
        .arg(pid.to_string())
        .status()
        .await?;

    kill(Pid::from_raw(pid as i32), SIGINT)?;

    // Verify the results
    assert!(!rss_res.is_empty());
    println!("Got VmRSS: {} KB", rss_res);

    let rss = rss_res.parse::<u32>()?;
    assert!(rss <= MAX_RSS_KB);

    Ok(())
}

/// Returns the directory where the binaries are saved. This
/// was taken from the Cargo project, see
/// https://github.com/rust-lang/cargo/blob/7fa132c7272fb9faca365c1d350e8e3c4c0d45e9/tests/cargotest/support/mod.rs#L316-L333
fn cargo_dir() -> PathBuf {
    env::var_os("CARGO_BIN_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            env::current_exe().ok().map(|mut path| {
                path.pop();
                if path.ends_with("deps") {
                    path.pop();
                }
                path
            })
        })
        .unwrap_or_else(|| panic!("could not find CARGO_BIN_PATH directory"))
}

fn server_binary() -> PathBuf {
    let binary_dir = cargo_dir();
    binary_dir.join(SERVER_BINARY)
}

fn client_binary() -> PathBuf {
    let binary_dir = cargo_dir();
    binary_dir.join(CLIENT_BINARY)
}
