use anyhow::{Result, bail};
use nix::{
    fcntl::{OFlag, open},
    sys::stat::{Mode, umask},
    unistd::write,
};
use std::{
    env,
    fs::OpenOptions,
    io::{ErrorKind, Write},
    path::Path,
    sync::OnceLock,
};
use tokio::process::Command;

/// The OOM score adjustment of the server.
const OOM_SCORE_ADJ: &str = "/proc/self/oom_score_adj";

/// Adjust the OOM score of the server, best effort without the privileges
/// to lower it.
pub fn set_oom_score(score: &str) -> Result<()> {
    set_oom_score_at(Path::new(OOM_SCORE_ADJ), score)
}

fn set_oom_score_at(path: &Path, score: &str) -> Result<()> {
    match OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|mut file| file.write_all(score.as_bytes()))
    {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            eprintln!("Missing sufficient privileges to adjust OOM score");
            Ok(())
        }
        Err(e) => bail!("adjusting OOM score {e}"),
    }
}

/// Set the default umask.
pub fn set_default_umask() {
    umask(Mode::from_bits_truncate(0o022));
}

/// Drop the sd_notify socket of whoever started the server. The server
/// does not talk to systemd itself, but every process it runs would
/// inherit the socket: the runtime then adds its own notify socket mount
/// and `NOTIFY_SOCKET` to the container, the cleanup commands and the
/// pause process get it as well. Callers set those up themselves and let
/// the server relay the messages, see the `notify` module.
#[expect(unsafe_code, reason = "changing the environment")]
pub fn unset_notify_socket() {
    // SAFETY: the server is still single threaded, the runtime and its
    // threads start later.
    unsafe { env::remove_var("NOTIFY_SOCKET") }
}

/// The most malloc arenas of the server. glibc gives every thread an arena
/// of its own, up to eight per CPU, and on AArch64 it backs them with 2 MiB
/// transparent huge pages by default, so each costs up to 2 MiB resident. The
/// runtime, which also runs the RPC `LocalSet`, runs on one thread, the others
/// only serve short blocking work, so one arena is enough, the per thread
/// caches of the allocator absorb most of the contention.
#[cfg(target_env = "gnu")]
const MALLOC_ARENAS: libc::c_int = 1;

/// The size from which on glibc maps every allocation on its own, which goes
/// back to the kernel once freed, and the free memory at the top of the heap
/// from which on glibc returns it. Both are the defaults of glibc, which
/// raises them to the size of every mapped allocation that gets freed, up to
/// 32 and 64 MiB. After an exec_sync with a large output, up to 16 MiB per
/// pipe which get copied into the response, the allocations of that size
/// would stay in the heap then and keep it resident. Setting them turns that
/// off.
#[cfg(target_env = "gnu")]
const MALLOC_THRESHOLD: libc::c_int = 128 * 1024;

/// Limit the malloc arenas of the server, see [`MALLOC_ARENAS`], and fix the
/// thresholds of glibc, see [`MALLOC_THRESHOLD`]. Runs before the fork, while
/// the process has a single thread. The runtimes and containers keep the
/// defaults, an exec starts their allocator anew.
#[cfg_attr(target_env = "gnu", expect(unsafe_code, reason = "mallopt"))]
pub fn configure_malloc() {
    #[cfg(target_env = "gnu")]
    for (param, value, name) in [
        (libc::M_ARENA_MAX, MALLOC_ARENAS, "arenas"),
        (libc::M_MMAP_THRESHOLD, MALLOC_THRESHOLD, "mmap threshold"),
        (libc::M_TRIM_THRESHOLD, MALLOC_THRESHOLD, "trim threshold"),
    ] {
        // SAFETY: mallopt only changes a parameter of the allocator.
        if unsafe { libc::mallopt(param, value) } != 1 {
            // Before the logging gets set up.
            eprintln!("Unable to set the malloc {name}");
        }
    }
}

/// The OOM score adjustment the server started with, see [`reset_oom_score`].
static INHERITED_OOM_SCORE: OnceLock<Box<[u8]>> = OnceLock::new();

/// Remember the OOM score adjustment the server started with, before it
/// protects itself with [`set_oom_score`].
pub fn save_oom_score() {
    match std::fs::read(OOM_SCORE_ADJ) {
        Ok(score) => {
            let _ = INHERITED_OOM_SCORE.set(score.trim_ascii().into());
        }
        Err(e) => eprintln!("Unable to read OOM score: {e}"),
    }
}

/// Give a child the OOM score adjustment the server started with instead of
/// the one it protects itself with. The runtimes pass theirs on to the
/// container processes, an exec would be the last process the kernel picks
/// otherwise. conmon resets its runtime the same way. Only for the runtimes
/// and cleanup commands, the pause process keeps the one of the server.
#[expect(unsafe_code, reason = "a pre_exec hook")]
pub fn reset_oom_score(cmd: &mut Command) {
    let Some(score) = INHERITED_OOM_SCORE.get() else {
        return;
    };
    let score: &'static [u8] = score;
    // SAFETY: the hook allocates nothing and only calls open, write and
    // close, which are async signal safe. Raising the score needs no
    // privileges, a failure keeps the inherited one, as before.
    unsafe {
        cmd.pre_exec(move || {
            if let Ok(fd) = open(
                c"/proc/self/oom_score_adj",
                OFlag::O_WRONLY | OFlag::O_CLOEXEC,
                Mode::empty(),
            ) {
                let _ = write(fd, score);
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn oom_score_gets_written() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("oom_score_adj");
        fs::write(&path, "0")?;
        set_oom_score_at(&path, "-1000")?;
        assert_eq!(fs::read_to_string(&path)?, "-1000");

        // Without the privileges it stays as it is.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
        set_oom_score_at(&path, "-999")?;

        // Anything else fails.
        assert!(set_oom_score_at(&dir.path().join("missing").join("file"), "0").is_err());
        assert!(set_oom_score_at(dir.path(), "0").is_err());
        Ok(())
    }
}
