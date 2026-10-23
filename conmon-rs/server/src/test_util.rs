//! Helpers shared by the tests.

use anyhow::{Context, Result, ensure};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
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
