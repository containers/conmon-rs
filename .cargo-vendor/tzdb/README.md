# tzdb — Time Zone Database

[![GitHub Workflow Status](https://img.shields.io/github/actions/workflow/status/Kijewski/tzdb/ci.yml?branch=v0.6.x&style=for-the-badge)](https://github.com/Kijewski/tzdb/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/tzdb?logo=rust&style=for-the-badge)](https://crates.io/crates/tzdb)
![Minimum supported Rust version](https://img.shields.io/badge/rustc-1.56+-important?logo=rust&style=for-the-badge "Minimum Supported Rust Version: 1.56")
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-informational?logo=apache&style=for-the-badge)](/LICENSE.md "License: Apache-2.0")

Static time zone information for [tz-rs](https://crates.io/crates/tz-rs).

This crate provides all time zones found in the [Time Zone Database](https://www.iana.org/time-zones).

## Usage examples

```rust
let time_zone = tzdb::local_tz()?;       // tz::TimeZoneRef<'_>
let current_time = tzdb::now::local()?;  // tz::DateTime

// access by identifier
let time_zone = tzdb::time_zone::europe::KYIV;
let current_time = tzdb::now::in_tz(tzdb::time_zone::europe::KYIV)?;

// access by name
let time_zone = tzdb::tz_by_name("Europe/Berlin")?;
let current_time = tzdb::now::in_named("Europe/Berlin")?;

// names are case insensitive
let time_zone = tzdb::tz_by_name("ArCtIc/LongYeArByEn")?;
let current_time = tzdb::now::in_named("ArCtIc/LoNgYeArByEn")?;

// provide a default time zone
let current_time = tzdb::now::local_or(tzdb::time_zone::GMT)?;
let current_time = tzdb::now::in_named_or(tzdb::time_zone::GMT, "Some/City")?;
```

## Feature flags

* `local` <sup>(enabled by default)</sup> — enable functions to query the current system time
