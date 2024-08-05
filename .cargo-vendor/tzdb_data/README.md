# tzdb_data — Time Zone Database

[![GitHub Workflow Status](https://img.shields.io/github/actions/workflow/status/Kijewski/tzdb/ci.yml?branch=v0.6.x&style=for-the-badge)](https://github.com/Kijewski/tzdb/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/tzdb_data?logo=rust&style=for-the-badge)](https://crates.io/crates/tzdb_data)
![Minimum supported Rust version](https://img.shields.io/badge/rustc-1.56+-important?logo=rust&style=for-the-badge "Minimum Supported Rust Version: 1.56")
[![License: MIT-0](https://img.shields.io/badge/license-MIT--0-informational?logo=apache&style=for-the-badge)](https://github.com/Kijewski/tzdb/blob/v0.6.1/tzdb_data/LICENSE.md "License: MIT-0")

Static, `#![no_std]` time zone information for tz-rs

## Usage examples

```rust
// access by identifier
let time_zone = tzdb_data::time_zone::europe::KYIV;
// access by name
let time_zone = tzdb_data::find_tz(b"Europe/Berlin").unwrap();
// names are case insensitive
let time_zone = tzdb_data::find_tz(b"ArCtIc/LoNgYeArByEn").unwrap();
```
