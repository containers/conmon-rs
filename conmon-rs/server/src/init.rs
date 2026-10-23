use anyhow::{Result, bail};
use nix::{
    errno::Errno,
    fcntl::{OFlag, open},
    sys::{
        prctl,
        resource::{Resource, getrlimit, rlim_t, setrlimit},
        stat::{Mode, umask},
    },
    unistd::write,
};
use std::{
    env,
    fs::OpenOptions,
    io::{ErrorKind, Write},
    path::Path,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
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

/// Whether the server turned transparent huge pages off for itself, which
/// the processes it runs must not inherit, see [`disable_thp`].
static THP_DISABLED: AtomicBool = AtomicBool::new(false);

/// Turn transparent huge pages off for the process, as early as possible.
/// On AArch64 glibc asks for them for its heap by default, so the first
/// growth of the heap beyond a 2 MiB boundary costs 2 MiB resident, which a
/// server per pod does not need. mallopt cannot turn that off, and the
/// tunable which can takes effect only from the start of the process. Huge
/// pages which were already allocated stay.
///
/// The setting gets inherited and kept across exec, so the runtimes and the
/// cleanup commands get the one the server started with, see
/// [`reset_inherited`]. A setting of whoever started the server stays.
pub fn disable_thp() {
    match prctl::get_thp_disable() {
        Ok(false) => {}
        // Already disabled, or not supported, like under qemu-user.
        Ok(true) | Err(Errno::EINVAL) => return,
        Err(e) => {
            // Before the logging gets set up.
            eprintln!("Unable to get the transparent huge pages setting: {e}");
            return;
        }
    }
    match prctl::set_thp_disable(true) {
        Ok(()) => THP_DISABLED.store(true, Ordering::Relaxed),
        Err(e) => eprintln!("Unable to disable transparent huge pages: {e}"),
    }
}

/// The OOM score adjustment the server started with, see [`reset_inherited`].
static INHERITED_OOM_SCORE: OnceLock<Box<[u8]>> = OnceLock::new();

/// The soft and hard limit of open files the server started with, if it
/// raised the soft one, see [`raise_nofile_limit`].
static INHERITED_NOFILE: OnceLock<(rlim_t, rlim_t)> = OnceLock::new();

/// Raise the soft limit of open files to the hard one. The server holds the
/// pipes, log files, pidfds and sockets of every container of a pod and the
/// descriptors which clients pass, while Go programs like CRI-O start it
/// with the soft limit they started with, often 1024. The runtimes and the
/// cleanup commands get the limits the server started with, see
/// [`reset_inherited`].
pub fn raise_nofile_limit() {
    let (soft, hard) = match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok(limits) => limits,
        Err(e) => {
            // Before the logging gets set up.
            eprintln!("Unable to get the open files limit: {e}");
            return;
        }
    };
    if soft >= hard {
        return;
    }
    match setrlimit(Resource::RLIMIT_NOFILE, hard, hard) {
        Ok(()) => {
            let _ = INHERITED_NOFILE.set((soft, hard));
        }
        Err(e) => eprintln!("Unable to raise the open files limit to {hard}: {e}"),
    }
}

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

/// Give a child the OOM score adjustment, the transparent huge pages setting
/// and the open files limits the server started with instead of its own, see
/// [`save_oom_score`], [`disable_thp`] and [`raise_nofile_limit`]. The
/// runtimes pass them on to the container processes, an exec would be the
/// last process the kernel picks otherwise. conmon resets the OOM score of
/// its runtime the same way. Only for the runtimes and cleanup commands, the
/// pause process keeps the ones of the server. Add it after the other
/// pre_exec hooks, those run in order and may need the raised limit.
#[expect(unsafe_code, reason = "a pre_exec hook")]
pub fn reset_inherited(cmd: &mut Command) {
    let score: Option<&'static [u8]> = INHERITED_OOM_SCORE.get().map(|score| &**score);
    let thp_disabled = THP_DISABLED.load(Ordering::Relaxed);
    let nofile = INHERITED_NOFILE.get().copied();
    if score.is_none() && !thp_disabled && nofile.is_none() {
        return;
    }
    // SAFETY: the hook allocates nothing and only calls open, write, close,
    // prctl and setrlimit, which are async signal safe. Raising the score,
    // enabling huge pages and lowering a limit need no privileges, a
    // failure keeps the setting of the server, as before.
    unsafe {
        cmd.pre_exec(move || {
            if let Some(score) = score
                && let Ok(fd) = open(
                    c"/proc/self/oom_score_adj",
                    OFlag::O_WRONLY | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
            {
                let _ = write(fd, score);
            }
            if thp_disabled {
                let _ = prctl::set_thp_disable(false);
            }
            restore_nofile(nofile);
            Ok(())
        });
    }
}

/// Set the open files limits of the process, if any, async signal safe for a
/// pre_exec hook. A failure keeps the ones of the server.
fn restore_nofile(limits: Option<(rlim_t, rlim_t)>) {
    if let Some((soft, hard)) = limits {
        let _ = setrlimit(Resource::RLIMIT_NOFILE, soft, hard);
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

    #[tokio::test]
    #[expect(unsafe_code, reason = "a pre_exec hook")]
    async fn restore_nofile_sets_the_child_limit() -> Result<()> {
        // Like a server which raised the soft limit it started with. Only the
        // child gets the lower one, the limits of the tests stay.
        let limits = getrlimit(Resource::RLIMIT_NOFILE)?;
        let inherited = limits.0.min(4096) / 2;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "ulimit -Sn"]);
        // SAFETY: the hook only calls setrlimit, see `reset_inherited`.
        unsafe {
            cmd.pre_exec(move || {
                restore_nofile(Some((inherited, limits.1)));
                Ok(())
            });
        }
        let output = cmd.output().await?;
        assert_eq!(
            String::from_utf8(output.stdout)?.trim(),
            inherited.to_string()
        );
        assert_eq!(getrlimit(Resource::RLIMIT_NOFILE)?, limits);
        Ok(())
    }

    /// The transparent huge pages setting of a process, true if enabled.
    fn thp_enabled(status: &str) -> Option<bool> {
        let line = status.lines().find(|l| l.starts_with("THP_enabled:"))?;
        Some(line.split_whitespace().nth(1)? == "1")
    }

    async fn child_thp_enabled(reset: bool) -> Result<Option<bool>> {
        let mut cmd = Command::new("cat");
        cmd.arg("/proc/self/status");
        if reset {
            reset_inherited(&mut cmd);
        }
        let output = cmd.output().await?;
        Ok(thp_enabled(&String::from_utf8(output.stdout)?))
    }

    /// Changes the setting of the test process, which only its children
    /// inherit, and restores it afterwards. The runtimes which other tests
    /// spawn meanwhile get it reset, which does not matter to them.
    #[tokio::test]
    async fn runtimes_do_not_inherit_disabled_thp() -> Result<()> {
        if thp_enabled(&fs::read_to_string("/proc/self/status")?) != Some(true)
            || prctl::get_thp_disable() != Ok(false)
        {
            // Disabled by whoever started the tests, which stays, or not
            // supported by the kernel.
            return Ok(());
        }
        disable_thp();
        let disabled = thp_enabled(&fs::read_to_string("/proc/self/status")?);
        let inherited = child_thp_enabled(false).await;
        let reset = child_thp_enabled(true).await;
        prctl::set_thp_disable(false)?;
        THP_DISABLED.store(false, Ordering::Relaxed);

        assert_eq!(disabled, Some(false));
        assert_eq!(inherited?, Some(false));
        assert_eq!(reset?, Some(true));
        Ok(())
    }
}
