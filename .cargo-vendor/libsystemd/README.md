# libsystemd

[![crates.io](https://img.shields.io/crates/v/libsystemd.svg)](https://crates.io/crates/libsystemd)
[![LoC](https://tokei.rs/b1/github/lucab/libsystemd-rs?category=code)](https://github.com/lucab/libsystemd-rs)
[![Documentation](https://docs.rs/libsystemd/badge.svg)](https://docs.rs/libsystemd)

A pure-Rust client library to work with systemd.

It provides support to interact with systemd components available
on modern Linux systems. This crate is entirely implemented
in Rust, and does not require the libsystemd C library.

NB: this crate is not yet features-complete. If you don't care about C dependency, check [rust-systemd](https://github.com/jmesmon/rust-systemd) instead.

## Example

```rust
extern crate libsystemd;
use libsystemd::daemon::{self, NotifyState};

fn main() {
    if !daemon::booted() {
        panic!("Not running systemd, early exit.");
    };

    let sent = daemon::notify(true, &[NotifyState::Ready]).expect("notify failed");
    if !sent {
        panic!("Notification not sent, early exit.");
    };
    std::thread::park();
}
```

Some more examples are available under [examples](examples).

## License

Licensed under either of

 * MIT license - <http://opensource.org/licenses/MIT>
 * Apache License, Version 2.0 - <http://www.apache.org/licenses/LICENSE-2.0>

at your option.
