use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, Write};
use std::os::unix::prelude::{AsRawFd, FromRawFd, IntoRawFd};
use std::process::Command;
use std::result::Result;

use libsystemd::activation;
use libsystemd::daemon::{self, NotifyState};

const PERSISTENT_STATE: &[u8] = "STATE".as_bytes();

/// Create a memory backed file as state and store it in systemd.
fn create_and_store_persistent_state() -> Result<File, Box<dyn Error>> {
    let path = format!("/dev/shm/persistent_state-{}", std::process::id());
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)?;
    fs::remove_file(&path)?;

    let nss = [
        NotifyState::Fdname("persistent-state".to_owned()),
        NotifyState::Fdstore,
    ];

    daemon::notify_with_fds(false, &nss, &[f.as_raw_fd()])?;
    f.write_all(PERSISTENT_STATE)?;
    f.rewind()?;
    Ok(f)
}

fn run() -> Result<i32, Box<dyn Error>> {
    if !daemon::booted() {
        println!("Not running systemd, early exit.");
        return Ok(1);
    };

    let mut descriptors =
        activation::receive_descriptors_with_names(false).unwrap_or_else(|_| Vec::new());

    let mut persistent_state = if let Some((fd, name)) = descriptors.pop() {
        println!("Fetched persistent state from systemd");
        if name == "persistent-state" {
            unsafe { File::from_raw_fd(fd.into_raw_fd()) }
        } else {
            let err = io::Error::new(ErrorKind::Other, "Got the wrong file descriptor.");
            return Err(Box::new(err));
        }
    } else {
        println!("Got nothing from systemd, create new persistent state");
        create_and_store_persistent_state()?;
        // Systemd should have been configured to restart this process on exit status 2.
        return Ok(2);
    };

    // Read and increment state
    let mut stored_state = [0x0u8; PERSISTENT_STATE.len()];
    persistent_state.read_exact(&mut stored_state)?;

    assert_eq!(stored_state, PERSISTENT_STATE);

    let nss = [
        NotifyState::Fdname("persistent-state".to_owned()),
        NotifyState::FdstoreRemove,
    ];

    daemon::notify(false, &nss)?;
    println!("Exiting with success");

    Ok(0)
}
enum Journal {
    User,
    System,
}

fn main() -> Result<(), Box<dyn Error>> {
    // Run the example if we are reexecuted by the test.
    // Read on to understand.
    if std::env::var_os("RUN_EXAMPLE").is_some() {
        std::process::exit(run()?);
    }

    // On Github Actions use the system instance for this test, because there's
    // no user instance running apparently.
    let journal_instance = if std::env::var_os("GITHUB_ACTIONS").is_some() {
        Journal::System
    } else {
        Journal::User
    };

    // Restart this binary under systemd-run and then check the exit status.
    let exe = std::env::current_exe().unwrap();
    let status = match journal_instance {
        Journal::User => {
            let mut cmd = Command::new("systemd-run");
            cmd.arg("--user");
            cmd
        }
        Journal::System => {
            let mut cmd = Command::new("sudo");
            cmd.arg("systemd-run");
            cmd
        }
    }
    // Set environment so that the example will run instead.
    .arg("--setenv=RUN_EXAMPLE=1")
    // Make sure the example can store a filedescriptor and can be purposefully restarted.
    .arg("-pFileDescriptorStoreMax=1")
    .arg("-pRestartForceExitStatus=2")
    .arg("-pSuccessExitStatus=2")
    // Wait until the process exited and unload the entire unit afterwards to
    // leave no state behind
    .arg("--wait")
    .arg("--collect")
    .arg(exe)
    .status()?;

    assert!(status.success());
    Ok(())
}
