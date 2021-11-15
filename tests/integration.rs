use anyhow::{Context, Result};
use nix::{
    sys::signal::{kill, SIGINT},
    unistd::Pid,
};
use tokio::{
    fs,
    process::Command,
    time::{sleep, Duration},
};

// The maximum amount of allowed VmRSS in Kilobytes
const MAX_RSS_KB: u32 = 3200;

// We assume that the tests run in release mode
const SERVER_BINARY: &str = "target/release/conmon-server";
const CLIENT_BINARY: &str = "target/release/conmon-client";

#[tokio::test]
async fn rss_verification() -> Result<()> {
    // Start the server
    let mut server = Command::new(SERVER_BINARY).arg("--runtime=/tmp").spawn()?;
    let pid = server.id().context("no pid for child")?;
    tokio::spawn(async move {
        server.wait().await.expect("wait for server process");
    });

    // Wait for the server up and running
    for i in 1..101 {
        let status = Command::new(CLIENT_BINARY).status().await?;
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
    assert!(rss_res != "");
    println!("Got VmRSS: {} KB", rss_res);

    let rss = rss_res.parse::<u32>()?;
    assert!(rss <= MAX_RSS_KB);

    Ok(())
}
