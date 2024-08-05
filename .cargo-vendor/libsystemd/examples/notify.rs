extern crate libsystemd;

use libsystemd::daemon::{self, NotifyState};
use std::{env, thread};

/*
cargo build --example notify
systemd-run --user --wait -p Type=notify ./target/debug/examples/notify [CUSTOM_STATUS]
*/

fn main() {
    if !daemon::booted() {
        println!("Not running systemd, early exit.");
        return;
    };
    let state = match env::args().nth(1) {
        Some(s) => NotifyState::Status(s),
        None => NotifyState::Ready,
    };

    let sent = daemon::notify(true, &[state]).expect("notify failed");
    if !sent {
        println!("Notification not sent!");
    }

    thread::park();
}
