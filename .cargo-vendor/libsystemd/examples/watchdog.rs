extern crate libsystemd;

use libsystemd::daemon::{self, NotifyState};
use std::thread;

/*
```
[Service]
WatchdogSec=1s
ExecStart=/home/user/libsystemd-rs/target/debug/examples/watchdog
```

cargo build --example watchdog ; systemctl start --wait --user watchdog; systemctl status --user watchdog
*/

fn main() {
    if !daemon::booted() {
        println!("Not running systemd, early exit.");
        return;
    };

    let timeout = daemon::watchdog_enabled(true).expect("watchdog disabled");
    for i in 0..20 {
        let _sent = daemon::notify(false, &[NotifyState::Watchdog]).expect("notify failed");
        println!("Notification #{} sent...", i);
        thread::sleep(timeout / 2);
    }

    println!("Blocking forever!");
    thread::park();
}
