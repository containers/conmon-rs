use anyhow::{bail, Result};
use gettextrs::{setlocale, LocaleCategory};
use log::info;
use std::fs::File;
use std::io::{ErrorKind, Write};

/// Unset the locale for the current process.
pub fn unset_locale() {
    setlocale(LocaleCategory::LcAll, "");
}

/// Helper to adjust the OOM score of the currently running process.
pub fn set_oom(score: &str) -> Result<()> {
    // Attempt adjustment with best-effort.
    if let Err(err) = File::create("/proc/self/oom_score_adj")?.write_all(score.as_bytes()) {
        match err.kind() {
            ErrorKind::PermissionDenied => {
                info!("Missing sufficient privileges to adjust OOM score")
            }
            _ => bail!("adjusting OOM score {}", err),
        }
    }
    Ok(())
}
