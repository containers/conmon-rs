use super::*;
use anyhow::ensure;
use libc::pid_t;

#[test]
fn init_signals() {
    let signals = Pause::init_signals();
    for signal in [
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGCHLD,
    ] {
        assert!(signals.contains(signal));
    }
    assert!(!signals.contains(Signal::SIGHUP));
}

#[test]
fn mappings_buffer() -> Result<()> {
    let mappings = |m: &[&str]| m.iter().map(ToString::to_string).collect::<Vec<_>>();

    // All ranges end up in one buffer, normalized to one line each.
    assert_eq!(
        Pause::mappings_buffer(&mappings(&["0 1000 1", "1\t100000  65536"]), 4096)?,
        "0 1000 1\n1 100000 65536\n"
    );
    assert_eq!(Pause::mappings_buffer(&[], 4096)?, "");
    // Nothing gets opened or written without mappings.
    Pause::write_mappings(&[], Pid::from_raw(pid_t::MAX), false)?;

    for invalid in [&["0 1000"][..], &["0 1000 1 2"], &["0 1000 x"], &["0 -1 1"]] {
        assert!(Pause::mappings_buffer(&mappings(invalid), 4096).is_err());
    }

    // A later invalid mapping fails before anything gets written.
    assert!(Pause::mappings_buffer(&mappings(&["0 0 1", "1 x 1"]), 4096).is_err());

    // The kernel takes less than a page and a limited number of ranges.
    let line = "4294967295 4294967295 1";
    let many = vec![line.to_string(); Pause::MAX_MAPPINGS];
    let err = Pause::mappings_buffer(&many, 4096).unwrap_err();
    assert!(format!("{err:#}").contains("less than 4096"), "{err:#}");
    assert!(Pause::mappings_buffer(&many, 65536).is_ok());
    let too_many = vec!["0 0 1".to_string(); Pause::MAX_MAPPINGS + 1];
    let err = Pause::mappings_buffer(&too_many, 65536).unwrap_err();
    assert!(format!("{err:#}").contains("limit of 340"), "{err:#}");
    Ok(())
}

#[test]
fn init_root_wait_ends_with_the_parent() -> Result<()> {
    let (mut init_sock, mut init) = UnixStream::pair()?;
    let (parent, child) = UnixStream::pair()?;
    init.write_all(Pause::MSG)?;
    Pause::wait_for_init_root(&mut init_sock, &child)?;

    // The parent got killed while the init still sets up its root.
    drop(parent);
    let err = Pause::wait_for_init_root(&mut init_sock, &child).unwrap_err();
    assert!(
        format!("{err:#}").contains("pause process is gone"),
        "{err:#}"
    );

    // The init died before it reported.
    let (_parent, child) = UnixStream::pair()?;
    drop(init);
    assert!(Pause::wait_for_init_root(&mut init_sock, &child).is_err());
    Ok(())
}

#[test]
fn root_has_parent_mount() {
    let line = |id, parent, mount_point| {
        format!("{id} {parent} 254:2 / {mount_point} rw - ext4 /dev/vda rw\n")
    };
    // A root mounted on the root mount of the namespace, which is not shown.
    let host = [line(43, 1, "/"), line(45, 43, "/run")].concat();
    assert!(Pause::root_has_parent_mount(&host));
    // An initramfs is the root mount itself.
    assert!(!Pause::root_has_parent_mount(&line(1, 1, "/")));
    // A chroot into a directory which is not a mount point.
    assert!(!Pause::root_has_parent_mount(&line(45, 43, "/run")));
    assert!(!Pause::root_has_parent_mount(""));
    // The topmost of stacked mounts counts, in any order.
    let stacked = [line(50, 1, "/"), line(1, 1, "/")].concat();
    assert!(Pause::root_has_parent_mount(&stacked));
    let stacked = [line(50, 50, "/"), line(43, 50, "/")].concat();
    assert!(Pause::root_has_parent_mount(&stacked));
    // Lines with fewer fields get skipped.
    assert!(!Pause::root_has_parent_mount("43 1 254:2 /\n"));
}

/// The init of a PID namespace does not reach the root of the host through
/// its `/proc/<pid>/root`, has no capabilities, is not dumpable once the
/// namespaces got bound, and the mounts of the host stay as they are. Needs
/// root, run it like `sudo -n <test binary> pause --ignored
/// --test-threads=1`, or with `unshare -rm` for a user namespace. CI runs
/// it as root in the unit test job.
#[test]
#[ignore = "needs root"]
#[expect(unsafe_code, reason = "forking")]
fn init_gets_an_empty_root() -> Result<()> {
    use nix::sys::statvfs::{FsFlags, statvfs};
    use std::os::unix::fs::MetadataExt;

    let (mut rx, mut tx) = UnixStream::pair()?;
    // SAFETY: the harness is multi threaded, the child relies on no lock
    // which another thread may have held at the fork as long as the setup
    // succeeds: it makes syscalls only, reports and waits to get killed. A
    // failure gets formatted, which allocates, a hang of the test would
    // show it then. It never returns into the test harness.
    let child = match unsafe { fork()? } {
        ForkResult::Parent { child } => child,
        ForkResult::Child => {
            let setup = Pause::enter_empty_root().and_then(|()| Pause::drop_privileges());
            // Dumpable until the binds, which the parent stands in for.
            let dumpable = prctl::get_dumpable();
            let bound = Pause::wait_for_binds(&mut tx);
            let checked = setup
                .and(bound)
                .and_then(|()| match (dumpable, prctl::get_dumpable()) {
                    (Ok(true), Ok(false)) => Ok(()),
                    (before, after) => {
                        bail!("dumpable {before:?} before the binds, {after:?} after")
                    }
                });
            let msg = checked.err().map(|e| format!("{e:#}")).unwrap_or_default();
            let _ = tx.write_all(msg.as_bytes());
            drop(tx);
            loop {
                nix::unistd::pause();
            }
        }
    };
    drop(tx);
    let mut reported = [0];
    let mut msg = String::new();
    let read = rx
        .read_exact(&mut reported)
        .and_then(|()| rx.shutdown(Shutdown::Write))
        .and_then(|()| rx.read_to_string(&mut msg));

    let root = PathBuf::from(format!("/proc/{child}/root"));
    let checked = (|| -> Result<()> {
        read?;
        ensure!(reported == *Pause::MSG, "reported {reported:?}");
        if !msg.is_empty() {
            bail!("set up init: {msg}")
        }
        let (host, init) = (fs::metadata("/")?, fs::metadata(&root)?);
        // Errors instead of panics, which would leave the child running.
        ensure!(
            (host.dev(), host.ino()) != (init.dev(), init.ino()),
            "host root"
        );
        ensure!(fs::read_dir(&root)?.next().is_none(), "root not empty");
        // A chroot would lead from the tmpfs to the host root here.
        let parent = root.join("..");
        ensure!(fs::read_dir(parent)?.next().is_none(), "parent not empty");
        let flags = statvfs(&root)?.flags();
        ensure!(flags.contains(FsFlags::ST_RDONLY), "root writable");
        // The tmpfs stayed in the mount namespace of the child.
        fs::metadata("/proc/self/status")?;

        let field = |status: &str, field: &str| -> Result<String> {
            let line = status
                .lines()
                .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
                .with_context(|| format!("no {field} in status"))?;
            Ok(line.trim().to_owned())
        };
        // Without CAP_SETPCAP, like with `setpriv --bounding-set=-setpcap`,
        // the bounding set stays, which does not fail the setup.
        let own = fs::read_to_string("/proc/self/status")?;
        let own_caps = u64::from_str_radix(&field(&own, "CapEff")?, 16)?;
        // CAP_SETPCAP is capability 8, see capabilities(7).
        let setpcap = own_caps & (1 << 8) != 0;
        let status = fs::read_to_string(format!("/proc/{child}/status"))?;
        for (name, value) in [
            ("CapInh", "0000000000000000"),
            ("CapPrm", "0000000000000000"),
            ("CapEff", "0000000000000000"),
            ("CapBnd", "0000000000000000"),
            ("CapAmb", "0000000000000000"),
            ("NoNewPrivs", "1"),
        ] {
            let found = field(&status, name)?;
            ensure!(
                found == value || name == "CapBnd" && !setpcap,
                "{name}: {found}"
            );
        }
        Ok(())
    })();
    kill(child, Signal::SIGKILL)?;
    waitpid(child, None)?;
    checked
}
