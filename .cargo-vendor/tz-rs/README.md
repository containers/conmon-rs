# tz-rs

[![version](https://img.shields.io/crates/v/tz-rs?color=blue&style=flat-square)](https://crates.io/crates/tz-rs)
![Minimum supported Rust version](https://img.shields.io/badge/rustc-1.45+-important?logo=rust "Minimum Supported Rust Version")
[![Documentation](https://docs.rs/tz-rs/badge.svg)](https://docs.rs/tz-rs)

A pure Rust reimplementation of libc functions [`localtime`](https://en.cppreference.com/w/c/chrono/localtime), [`gmtime`](https://en.cppreference.com/w/c/chrono/gmtime) and [`mktime`](https://en.cppreference.com/w/c/chrono/mktime).

This crate allows to convert between a [Unix timestamp](https://en.wikipedia.org/wiki/Unix_time) and a calendar time exprimed in the [proleptic gregorian calendar](https://en.wikipedia.org/wiki/Proleptic_Gregorian_calendar) with a provided time zone.

Time zones are provided to the library with a [POSIX `TZ` string](https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/V1_chap08.html) which can be read from the environment.

Two formats are currently accepted for the `TZ` string:
* `std offset[dst[offset][,start[/time],end[/time]]]` providing a time zone description,
* `file` or `:file` providing the path to a [TZif file](https://datatracker.ietf.org/doc/html/rfc8536), which is absolute or relative to the system timezone directory.

See also the [Linux manual page of tzset(3)](https://man7.org/linux/man-pages/man3/tzset.3.html) and the [glibc documentation of the `TZ` environment variable](https://www.gnu.org/software/libc/manual/html_node/TZ-Variable.html).

## Context

Calls to libc `localtime_r` and other related functions from Rust are not safe in a multithreaded application, because they may internally set the `TZ` environment variable with the `setenv` function, which is not thread-safe.

See [RUSTSEC-2020-0071](https://rustsec.org/advisories/RUSTSEC-2020-0071.html) and [RUSTSEC-2020-0159](https://rustsec.org/advisories/RUSTSEC-2020-0159.html) for more information.

## Documentation

Documentation is hosted on [docs.rs](https://docs.rs/tz-rs/latest/tz/).

## Platform support

This crate is mainly intended for UNIX platforms.

Since the time zone database files are not included in this crate, non-UNIX users can download a copy of the database on the [IANA site](https://www.iana.org/time-zones) and compile the time zone database files to a local directory.

The database files can then be read by specifying an absolute path in the `TZ` string:

```rust
TimeZone::from_posix_tz(format!("{local_database_dir}/usr/share/zoneinfo/Pacific/Auckland"))?;
```

Note that the determination of the local time zone with this crate is not supported on non-UNIX platforms.

Alternatively, a crate like [tzdb](https://github.com/Kijewski/tzdb) can be used, which statically provides existing time zone definitions for this crate, and supports finding the local time zone for all [Tier 1](https://doc.rust-lang.org/nightly/rustc/platform-support.html) platforms.

## Date time formatting (equivalent of libc `strftime` function)

This crate doesn't provide custom date time formatting support, but the [`custom-format`](https://github.com/x-hgg-x/custom-format) crate can be used to provide custom format specifiers to the standard library formatting macros.

## Compiler support

Requires `rustc 1.45+` when building with no default features, or `rustc 1.55+` otherwise.

## License

This project is licensed under either of

- [Apache License, Version 2.0](https://github.com/x-hgg-x/tz-rs/blob/master/LICENSE-Apache)
- [MIT license](https://github.com/x-hgg-x/tz-rs/blob/master/LICENSE-MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
