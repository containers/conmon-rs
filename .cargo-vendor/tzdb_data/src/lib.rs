// SPDX-License-Identifier: MIT-0
//
// Copyright 2022-2024 René Kijewski <crates.io@k6i.de>

#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unknown_lints)]
#![forbid(unsafe_code)]
#![warn(absolute_paths_not_starting_with_crate)]
#![warn(elided_lifetimes_in_paths)]
#![warn(explicit_outlives_requirements)]
#![warn(meta_variable_misuse)]
#![warn(missing_copy_implementations)]
#![warn(missing_debug_implementations)]
#![warn(missing_docs)]
#![warn(non_ascii_idents)]
#![warn(noop_method_call)]
#![warn(single_use_lifetimes)]
#![warn(trivial_casts)]
#![warn(unreachable_pub)]
#![warn(unused_extern_crates)]
#![warn(unused_lifetimes)]
#![warn(unused_results)]
#![allow(clippy::single_match_else)]
#![allow(clippy::type_complexity)]
#![no_std]

//! # `tzdb_data` — Time Zone Database
//!
//! [![GitHub Workflow Status](https://img.shields.io/github/actions/workflow/status/Kijewski/tzdb/ci.yml?branch=v0.6.x&style=for-the-badge)](https://github.com/Kijewski/tzdb/actions/workflows/ci.yml)
//! [![Crates.io](https://img.shields.io/crates/v/tzdb_data?logo=rust&style=for-the-badge)](https://crates.io/crates/tzdb_data)
//! ![Minimum supported Rust version](https://img.shields.io/badge/rustc-1.56+-important?logo=rust&style=for-the-badge "Minimum Supported Rust Version: 1.56")
//! [![License: MIT-0](https://img.shields.io/badge/license-MIT--0-informational?logo=apache&style=for-the-badge)](https://github.com/Kijewski/tzdb/blob/v0.6.1/tzdb_data/LICENSE.md "License: MIT-0")
//!
//! Static, `#![no_std]` time zone information for tz-rs
//!
//! This crate provides all time zones found in the [Time Zone Database](https://www.iana.org/time-zones).
//!
//! ## Usage examples
//!
//! ```rust
//! // access by identifier
//! let time_zone = tzdb_data::time_zone::europe::KYIV;
//! // access by name
//! let time_zone = tzdb_data::find_tz(b"Europe/Berlin").unwrap();
//! // names are case insensitive
//! let time_zone = tzdb_data::find_tz(b"ArCtIc/LoNgYeArByEn").unwrap();
//! ```
//!

mod generated;

#[cfg_attr(docsrs, doc(inline))]
pub use crate::generated::{time_zone, TZ_NAMES, VERSION, VERSION_HASH};

/// Find a time zone by name, e.g. `b"Europe/Berlin"` (case-insensitive)
///
/// # Example
///
/// ```
/// assert_eq!(
///     &tzdb_data::time_zone::europe::BERLIN,
///     tzdb_data::find_tz(b"Europe/Berlin").unwrap(),
/// );
/// ```
#[inline]
#[must_use]
pub const fn find_tz(s: &[u8]) -> Option<&'static tz::TimeZoneRef<'static>> {
    match generated::by_name::find_key(s) {
        Some(key) => Some(generated::by_name::TIME_ZONES[key as u16 as usize]),
        None => None,
    }
}

/// Find the raw, unparsed time zone data by name, e.g. `b"Europe/Berlin"` (case-insensitive)
///
/// # Example
///
/// ```
/// assert_eq!(
///     tzdb_data::time_zone::europe::RAW_BERLIN,
///     tzdb_data::find_raw(b"Europe/Berlin").unwrap(),
/// );
/// ```
#[inline]
#[must_use]
pub const fn find_raw(s: &[u8]) -> Option<&'static [u8]> {
    match generated::by_name::find_key(s) {
        Some(key) => Some(generated::by_name::RAW_TIME_ZONES[key as u16 as usize]),
        None => None,
    }
}

#[allow(clippy::out_of_bounds_indexing)]
#[must_use]
const fn new_time_zone_ref(
    transitions: &'static [tz::timezone::Transition],
    local_time_types: &'static [tz::LocalTimeType],
    leap_seconds: &'static [tz::timezone::LeapSecond],
    extra_rule: &'static Option<tz::timezone::TransitionRule>,
) -> tz::timezone::TimeZoneRef<'static> {
    match tz::timezone::TimeZoneRef::new(transitions, local_time_types, leap_seconds, extra_rule) {
        Ok(value) => value,
        Err(_) => {
            #[allow(unconditional_panic)]
            let err = [][0];
            err
        },
    }
}

#[allow(clippy::out_of_bounds_indexing)]
#[must_use]
const fn new_local_time_type(
    ut_offset: i32,
    is_dst: bool,
    time_zone_designation: Option<&[u8]>,
) -> tz::LocalTimeType {
    match tz::LocalTimeType::new(ut_offset, is_dst, time_zone_designation) {
        Ok(value) => value,
        Err(_) => {
            #[allow(unconditional_panic)]
            let err = [][0];
            err
        },
    }
}

#[must_use]
const fn new_transition(
    unix_leap_time: i64,
    local_time_type_index: usize,
) -> tz::timezone::Transition {
    tz::timezone::Transition::new(unix_leap_time, local_time_type_index)
}

#[allow(clippy::out_of_bounds_indexing)]
#[must_use]
const fn new_alternate_time(
    std: tz::LocalTimeType,
    dst: tz::LocalTimeType,
    dst_start: tz::timezone::RuleDay,
    dst_start_time: i32,
    dst_end: tz::timezone::RuleDay,
    dst_end_time: i32,
) -> tz::timezone::AlternateTime {
    match tz::timezone::AlternateTime::new(
        std,
        dst,
        dst_start,
        dst_start_time,
        dst_end,
        dst_end_time,
    ) {
        Ok(value) => value,
        Err(_) => {
            #[allow(unconditional_panic)]
            let err = [][0];
            err
        },
    }
}

#[allow(clippy::out_of_bounds_indexing)]
#[must_use]
const fn new_month_week_day(month: u8, week: u8, week_day: u8) -> tz::timezone::MonthWeekDay {
    match tz::timezone::MonthWeekDay::new(month, week, week_day) {
        Ok(value) => value,
        Err(_) => {
            #[allow(unconditional_panic)]
            let err = [][0];
            err
        },
    }
}

// This implementation allows for invalid equalities like `b'-' == b'\x7f'`, but that's OK.
//
// The only troublesome characters are:
//     @ -> `
//     [ -> {
//     \ -> |
//     ] -> }
//     ^ -> ~
//     _ -> DEL
//
// None the these characters have a "false lower case" variant which can occur in the input.
// This function is 40% faster than the variant in rust's core library, which is implemented
// more strictly.
#[inline]
#[must_use]
const fn eq_ignore_ascii_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    // Cannot use for-loops in const fn.
    let mut i = 0;
    while i < a.len() {
        if (a[i] | 0x20) != (b[i] | 0x20) {
            return false;
        }
        i += 1;
    }

    true
}
