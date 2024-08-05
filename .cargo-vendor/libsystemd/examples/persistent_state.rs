// File descriptor storing (FDSTORE) example.
//
// Store a piece of state in form of a file descriptor which will survive the life of the process
// by storing the file descriptor in systemd. The stored file descriptors are made available to
// the process on restart.
//
// The example demonstates this by storing a single byte in a memory backed file. The byte is
// sent to systemd and incremented before exiting. Upon automatic restart the state is retrieved
// from systemd and incremented again. If the state byte has been incremented a set amount of
// times (3) the example is considered finished.
//
// In this example systemd must be configured to restart only on certain exit statuses. See the
// example commands below.
//
// ```shell
// cargo build --example persistent_state
// systemd-run --user -p RestartForceExitStatus=2 -p SuccessExitStatus=2 -p FileDescriptorStoreMax=1 --wait ./target/debug/examples/persistent_state
// journalctl --user -ocat -xeu <unitname>.service
// ```
//
// The persistent behaviour can be observed in the journalctl log for the unit:
// ```shell
// Started ./target/debug/examples/persistent_state.
// Created new persistent state
// State was: 1, exit and restart
// <unitname>.service: Scheduled restart job, restart counter is at 1.
// Stopped ./target/debug/examples/persistent_state.
// Started ./target/debug/examples/persistent_state.
// Fetched persistent state from systemd
// State was: 2, exit and restart
// <unitname>.service: Scheduled restart job, restart counter is at 2.
// Stopped ./target/debug/examples/persistent_state.
// Started ./target/debug/examples/persistent_state.
// Fetched persistent state from systemd
// Exiting normally because persistent state is now 3
// ```
//
// Please note that this example is also part of the integration suite.
// See tests/persistent_store.rs. Specifically it is important that the test is updated
// if changes are made to the systemd-run command above.

use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, Write};
use std::os::unix::prelude::{AsRawFd, FromRawFd, IntoRawFd};
use std::result::Result;

use libsystemd::activation;
use libsystemd::daemon::{self, NotifyState};

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
    // Set initial state to 0
    let state = [0u8, 1];
    f.set_len(state.len() as u64)?;
    f.write_all(&state)?;
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
        create_and_store_persistent_state()?
    };

    // Read and increment state
    let mut buf = [0xEFu8; 1];
    persistent_state.read_exact(&mut buf)?;
    persistent_state.rewind()?;

    buf[0] += 1;
    persistent_state.write_all(&buf)?;
    persistent_state.rewind()?;

    // Restart a few times
    if buf[0] < 3 {
        println!("State was: {}, exit and restart", buf[0]);
        // Systemd should have been configured to restart this process on exit status 2.
        return Ok(2);
    }

    println!(
        "Exiting normally because persistent state is now {}",
        buf[0]
    );

    let nss = [
        NotifyState::Fdname("persistent-state".to_owned()),
        NotifyState::FdstoreRemove,
    ];

    daemon::notify(false, &nss)?;

    Ok(0)
}

pub fn main() -> Result<(), Box<dyn Error>> {
    std::process::exit(run()?);
}
