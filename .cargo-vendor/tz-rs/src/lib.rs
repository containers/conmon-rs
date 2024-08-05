#![deny(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! This crate provides the [`TimeZone`] and [`DateTime`] types, which can be used to determine local time on a given time zone.
//!
//! This allows to convert between an [Unix timestamp](https://en.wikipedia.org/wiki/Unix_time) and a calendar time exprimed in the [proleptic gregorian calendar](https://en.wikipedia.org/wiki/Proleptic_Gregorian_calendar) with a provided time zone.
//!
//! Time zones are provided to the library with a [POSIX `TZ` string](https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/V1_chap08.html) which can be read from the environment.
//!
//! Two formats are currently accepted for the `TZ` string:
//! * `std offset[dst[offset][,start[/time],end[/time]]]` providing a time zone description,
//! * `file` or `:file` providing the path to a [TZif file](https://datatracker.ietf.org/doc/html/rfc8536), which is absolute or relative to the system timezone directory.
//!
//! See also the [Linux manual page of tzset(3)](https://man7.org/linux/man-pages/man3/tzset.3.html) and the [glibc documentation of the `TZ` environment variable](https://www.gnu.org/software/libc/manual/html_node/TZ-Variable.html).
//!
//! # Usage
//!
//! ## Time zone
//!
//! ```rust
//! # fn main() -> Result<(), tz::TzError> {
//! # #[cfg(feature = "std")] {
//! use tz::TimeZone;
//!
//! // 2000-01-01T00:00:00Z
//! let unix_time = 946684800;
//!
//! // Get UTC time zone
//! let time_zone_utc = TimeZone::utc();
//! assert_eq!(time_zone_utc.find_local_time_type(unix_time)?.ut_offset(), 0);
//!
//! // Get fixed time zone at GMT-1
//! let time_zone_fixed = TimeZone::fixed(-3600)?;
//! assert_eq!(time_zone_fixed.find_local_time_type(unix_time)?.ut_offset(), -3600);
//!
//! // Get local time zone (UNIX only)
//! let time_zone_local = TimeZone::local()?;
//! // Get the current local time type
//! let _current_local_time_type = time_zone_local.find_current_local_time_type()?;
//!
//! // Get time zone from a TZ string:
//! // From an absolute file
//! let _ = TimeZone::from_posix_tz("/usr/share/zoneinfo/Pacific/Auckland");
//! // From a file relative to the system timezone directory
//! let _ = TimeZone::from_posix_tz("Pacific/Auckland");
//! // From a time zone description
//! TimeZone::from_posix_tz("HST10")?;
//! TimeZone::from_posix_tz("<-03>3")?;
//! TimeZone::from_posix_tz("NZST-12:00:00NZDT-13:00:00,M10.1.0,M3.3.0")?;
//! // Use a leading colon to force searching for a corresponding file
//! let _ = TimeZone::from_posix_tz(":UTC");
//! # }
//! # Ok(())
//! # }
//! ```
//!
//! ## Date time
//!
//! ```rust
//! # fn main() -> Result<(), tz::TzError> {
//! # #[cfg(feature = "std")] {
//! use tz::{DateTime, LocalTimeType, TimeZone, UtcDateTime};
//!
//! // Get the current UTC date time
//! let _current_utc_date_time = UtcDateTime::now()?;
//!
//! // Create a new UTC date time (2000-01-01T00:00:00.123456789Z)
//! let utc_date_time = UtcDateTime::new(2000, 1, 1, 0, 0, 0, 123_456_789)?;
//! assert_eq!(utc_date_time.year(), 2000);
//! assert_eq!(utc_date_time.month(), 1);
//! assert_eq!(utc_date_time.month_day(), 1);
//! assert_eq!(utc_date_time.hour(), 0);
//! assert_eq!(utc_date_time.minute(), 0);
//! assert_eq!(utc_date_time.second(), 0);
//! assert_eq!(utc_date_time.week_day(), 6);
//! assert_eq!(utc_date_time.year_day(), 0);
//! assert_eq!(utc_date_time.unix_time(), 946684800);
//! assert_eq!(utc_date_time.nanoseconds(), 123_456_789);
//! assert_eq!(utc_date_time.to_string(), "2000-01-01T00:00:00.123456789Z");
//!
//! // Create a new UTC date time from a Unix time with nanoseconds (2000-01-01T00:00:00.123456789Z)
//! let other_utc_date_time = UtcDateTime::from_timespec(946684800, 123_456_789)?;
//! assert_eq!(other_utc_date_time, utc_date_time);
//!
//! // Project the UTC date time to a time zone
//! let date_time = utc_date_time.project(TimeZone::fixed(-3600)?.as_ref())?;
//! assert_eq!(date_time.year(), 1999);
//! assert_eq!(date_time.month(), 12);
//! assert_eq!(date_time.month_day(), 31);
//! assert_eq!(date_time.hour(), 23);
//! assert_eq!(date_time.minute(), 0);
//! assert_eq!(date_time.second(), 0);
//! assert_eq!(date_time.week_day(), 5);
//! assert_eq!(date_time.year_day(), 364);
//! assert_eq!(date_time.local_time_type().ut_offset(), -3600);
//! assert_eq!(date_time.unix_time(), 946684800);
//! assert_eq!(date_time.nanoseconds(), 123_456_789);
//! assert_eq!(date_time.to_string(), "1999-12-31T23:00:00.123456789-01:00");
//!
//! // Project the date time to another time zone
//! let other_date_time = date_time.project(TimeZone::fixed(3600)?.as_ref())?;
//! assert_eq!(other_date_time.year(), 2000);
//! assert_eq!(other_date_time.month(), 1);
//! assert_eq!(other_date_time.month_day(), 1);
//! assert_eq!(other_date_time.hour(), 1);
//! assert_eq!(other_date_time.minute(), 0);
//! assert_eq!(other_date_time.second(), 0);
//! assert_eq!(other_date_time.week_day(), 6);
//! assert_eq!(other_date_time.year_day(), 0);
//! assert_eq!(other_date_time.local_time_type().ut_offset(), 3600);
//! assert_eq!(other_date_time.unix_time(), 946684800);
//! assert_eq!(other_date_time.nanoseconds(), 123_456_789);
//! assert_eq!(other_date_time.to_string(), "2000-01-01T01:00:00.123456789+01:00");
//!
//! // Create a new date time from a Unix time with nanoseconds and a time zone (2000-01-01T00:00:00.123456789Z)
//! let another_date_time = DateTime::from_timespec(946684800, 123_456_789, TimeZone::fixed(86400)?.as_ref())?;
//!
//! // DateTime objects are compared by their Unix time and nanoseconds
//! assert_eq!(another_date_time, other_date_time);
//!
//! // Get the current date time at the local time zone (UNIX only)
//! let time_zone_local = TimeZone::local()?;
//! let _date_time = DateTime::now(time_zone_local.as_ref())?;
//!
//! // Create a new date time with an UTC offset (2000-01-01T01:00:00.123456789+01:00)
//! let date_time = DateTime::new(2000, 1, 1, 1, 0, 0, 123_456_789, LocalTimeType::with_ut_offset(3600)?)?;
//! assert_eq!(date_time.year(), 2000);
//! assert_eq!(date_time.month(), 1);
//! assert_eq!(date_time.month_day(), 1);
//! assert_eq!(date_time.hour(), 1);
//! assert_eq!(date_time.minute(), 0);
//! assert_eq!(date_time.second(), 0);
//! assert_eq!(date_time.week_day(), 6);
//! assert_eq!(date_time.year_day(), 0);
//! assert_eq!(date_time.unix_time(), 946684800);
//! assert_eq!(date_time.nanoseconds(), 123_456_789);
//! assert_eq!(date_time.to_string(), "2000-01-01T01:00:00.123456789+01:00");
//!
//! //
//! // Find the possible date times corresponding to a date, a time and a time zone
//! //
//! let time_zone = TimeZone::from_posix_tz("CET-1CEST,M3.5.0,M10.5.0/3")?;
//!
//! // Found date time is unique
//! let found_date_times = DateTime::find(2000, 1, 1, 0, 0, 0, 0, time_zone.as_ref())?;
//! let unique = found_date_times.unique().unwrap();
//! assert_eq!(unique, found_date_times.earliest().unwrap());
//! assert_eq!(unique, found_date_times.latest().unwrap());
//! assert_eq!(unique.local_time_type().ut_offset(), 3600);
//! assert_eq!(unique.local_time_type().time_zone_designation(), "CET");
//!
//! // Found date time was skipped by a forward transition
//! let found_date_times = DateTime::find(2000, 3, 26, 2, 30, 0, 0, time_zone.as_ref())?;
//!
//! assert_eq!(found_date_times.unique(), None);
//!
//! let earliest = found_date_times.earliest().unwrap();
//! assert_eq!(earliest.hour(), 2);
//! assert_eq!(earliest.minute(), 0);
//! assert_eq!(earliest.local_time_type().ut_offset(), 3600);
//! assert_eq!(earliest.local_time_type().time_zone_designation(), "CET");
//!
//! let latest = found_date_times.latest().unwrap();
//! assert_eq!(latest.hour(), 3);
//! assert_eq!(latest.minute(), 0);
//! assert_eq!(latest.local_time_type().ut_offset(), 7200);
//! assert_eq!(latest.local_time_type().time_zone_designation(), "CEST");
//!
//! // Found date time is ambiguous because of a backward transition
//! let found_date_times = DateTime::find(2000, 10, 29, 2, 30, 0, 0, time_zone.as_ref())?;
//!
//! assert_eq!(found_date_times.unique(), None);
//!
//! let earliest = found_date_times.earliest().unwrap();
//! assert_eq!(earliest.hour(), 2);
//! assert_eq!(earliest.minute(), 30);
//! assert_eq!(earliest.local_time_type().ut_offset(), 7200);
//! assert_eq!(earliest.local_time_type().time_zone_designation(), "CEST");
//!
//! let latest = found_date_times.latest().unwrap();
//! assert_eq!(latest.hour(), 2);
//! assert_eq!(latest.minute(), 30);
//! assert_eq!(latest.local_time_type().ut_offset(), 3600);
//! assert_eq!(latest.local_time_type().time_zone_designation(), "CET");
//! # }
//! # Ok(())
//! # }
//! ```

#[cfg(feature = "alloc")]
extern crate alloc;

mod constants;
mod utils;

#[cfg(feature = "std")]
mod parse;

pub mod datetime;
pub mod error;
pub mod timezone;

#[doc(inline)]
pub use datetime::{DateTime, UtcDateTime};

#[doc(inline)]
pub use error::TzError;

#[doc(inline)]
pub use timezone::{LocalTimeType, TimeZoneRef};

#[doc(inline)]
#[cfg(feature = "alloc")]
pub use timezone::TimeZone;

/// Alias for [`std::result::Result`] with the crate unified error
pub type Result<T> = core::result::Result<T, TzError>;
