//! Helpers shared by the tests.

use anyhow::{Context, Result, ensure};
use std::{
    io::Write,
    net::Ipv4Addr,
    path::Path,
    process::{Command, Stdio},
};
use tokio::{
    net::TcpSocket,
    time::{self, Duration},
};

/// Write an executable shell script running `script`.
///
/// A file which the multi threaded test process wrote itself may still be
/// open for writing in a child which another thread forked meanwhile, then
/// executing it fails with ETXTBSY until that child execs. A process of its
/// own writes the file instead, so that this one never has it open.
pub fn write_script(path: &Path, script: &str) -> Result<()> {
    let mut child = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .context("spawn sh")?;
    // Closing stdin ends cat, the write may fail if sh failed early, which
    // its status tells.
    let written = child
        .stdin
        .take()
        .context("stdin of sh")?
        .write_all(format!("#!/bin/sh\n{script}\n").as_bytes());
    let status = child.wait()?;
    ensure!(
        status.success() && written.is_ok(),
        "write script {}: {status}, {written:?}",
        path.display()
    );
    Ok(())
}

/// Wait until `condition` holds, checking it every 10ms for at most 5s.
pub async fn wait_for(mut condition: impl FnMut() -> bool) -> Result<()> {
    time::timeout(Duration::from_secs(5), async {
        while !condition() {
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

/// A socket bound to a port which does not listen, so that connecting to
/// the port fails. It keeps the port, a closed one could be taken by
/// another test meanwhile.
pub fn closed_port() -> Result<(TcpSocket, u16)> {
    let socket = TcpSocket::new_v4()?;
    socket.bind((Ipv4Addr::LOCALHOST, 0).into())?;
    let port = socket.local_addr()?.port();
    Ok((socket, port))
}

/// A child which gets killed and reaped if a test fails before it did.
pub struct Guarded(pub std::process::Child);

impl Guarded {
    pub fn spawn(cmd: &mut Command) -> Result<Self> {
        Ok(Self(cmd.spawn()?))
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        // Both do nothing for a child which got waited for already.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
