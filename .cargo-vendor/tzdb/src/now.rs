//! Get the current time in some time zone

use std::convert::TryFrom;
use std::fmt;
use std::time::{SystemTime, SystemTimeError};

use tz::error::ProjectDateTimeError;
use tz::{DateTime, TimeZoneRef};

#[cfg(not(feature = "local"))]
mod iana_time_zone {
    #[allow(missing_copy_implementations)] // intentionally omitted
    #[derive(Debug)]
    #[non_exhaustive]
    pub struct GetTimezoneError(Impossible);

    #[derive(Debug, Clone, Copy)]
    enum Impossible {}
}

/// An error as returned by [`local()`] and similar functions
///
/// # See also:
///
/// * [`local()`] / [`local_or()`]
/// * [`in_named()`] / [`in_named_or()`]
/// * [`in_tz()`]
#[allow(clippy::module_name_repetitions)]
#[derive(Debug)]
pub enum NowError {
    /// Could not get time zone. Only returned by [`local()`].
    TimeZone(iana_time_zone::GetTimezoneError),
    /// Unknown system time zone. Only returned by [`local()`], and [`in_named()`].
    UnknownTimezone,
    /// Could not project timestamp.
    ProjectDateTime(ProjectDateTimeError),
    /// Could not get current system time.
    Utcnow(SystemTimeError),
}

impl fmt::Display for NowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TimeZone(_) => "could not get time zone",
            Self::UnknownTimezone => "unknown system time zone",
            Self::ProjectDateTime(_) => "could not project timestamp",
            Self::Utcnow(_) => "could not get current system time",
        })
    }
}

impl std::error::Error for NowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(feature = "local")]
            Self::TimeZone(err) => Some(err),
            #[cfg(not(feature = "local"))]
            Self::TimeZone(_) => None,
            Self::UnknownTimezone => None,
            Self::ProjectDateTime(err) => Some(err),
            Self::Utcnow(err) => Some(err),
        }
    }
}

/// Get the current time in the local system time zone
///
/// # Errors
///
/// Possible errors include:
///
/// * The current [Unix time](https://en.wikipedia.org/w/index.php?title=Unix_time&oldid=1101650731)
///   could not be determined.
/// * The current Unix time could not be projected into the time zone.
///   Most likely the system time is off, or you are a time traveler trying run this code a few billion years in the future or past.
/// * The local time zone could not be determined.
/// * The local time zone is not a valid [IANA time zone](https://www.iana.org/time-zones).
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), tzdb::now::NowError> {
/// // Query the time zone of the local system:
/// let now = tzdb::now::local()?;
/// # Ok(()) }
/// ```
///
/// In most cases you will want to default to a specified time zone if the system timezone
/// could not be determined. Then use e.g.
///
/// ```rust
/// # fn main() -> Result<(), tzdb::now::NowError> {
/// let now = tzdb::now::local_or(tzdb::time_zone::GMT)?;
/// # Ok(()) }
/// ```
///
/// # See also:
///
/// * `local()` / [`local_or()`]
/// * [`in_named()`] / [`in_named_or()`]
/// * [`in_tz()`]
#[cfg(feature = "local")]
#[cfg_attr(docsrs, doc(cfg(feature = "local")))]
pub fn local() -> Result<DateTime, NowError> {
    in_named(iana_time_zone::get_timezone().map_err(NowError::TimeZone)?)
}

/// Get the current time in the local system time zone with a fallback time zone
///
/// # Errors
///
/// Possible errors include:
///
/// * The current [Unix time](https://en.wikipedia.org/w/index.php?title=Unix_time&oldid=1101650731)
///   could not be determined.
/// * The current Unix time could not be projected into the time zone.
///   Most likely the system time is off, or you are a time traveler trying run this code a few billion years in the future or past.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), tzdb::now::NowError> {
/// // Query the time zone of the local system, or use GMT as default:
/// let now = tzdb::now::local_or(tzdb::time_zone::GMT)?;
/// # Ok(()) }
/// ```
///
/// # See also:
///
/// * [`local()`] / `local_or()`
/// * [`in_named()`] / [`in_named_or()`]
/// * [`in_tz()`]
#[cfg(feature = "local")]
#[cfg_attr(docsrs, doc(cfg(feature = "local")))]
pub fn local_or(default: TimeZoneRef<'_>) -> Result<DateTime, NowError> {
    let tz = iana_time_zone::get_timezone()
        .ok()
        .and_then(crate::tz_by_name)
        .unwrap_or(default);
    in_tz(tz)
}

/// Get the current time a given time zone
///
/// # Errors
///
/// Possible errors include:
///
/// * The current [Unix time](https://en.wikipedia.org/w/index.php?title=Unix_time&oldid=1101650731)
///   could not be determined.
/// * The current Unix time could not be projected into the time zone.
///   Most likely the system time is off, or you are a time traveler trying run this code a few billion years in the future or past.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), tzdb::now::NowError> {
/// // What is the time in Berlin?
/// let now = tzdb::now::in_tz(tzdb::time_zone::europe::BERLIN)?;
/// # Ok(()) }
/// ```
///
/// # See also:
///
/// * [`local()`] / [`local_or()`]
/// * [`in_named()`] / [`in_named_or()`]
/// * `in_tz()`
pub fn in_tz(time_zone_ref: TimeZoneRef<'_>) -> Result<DateTime, NowError> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(NowError::Utcnow)?;
    let secs = i64::try_from(now.as_secs()).map_err(|_| {
        NowError::ProjectDateTime(ProjectDateTimeError("now is too far in the future"))
    })?;
    let nanos = now.subsec_nanos();
    DateTime::from_timespec(secs, nanos, time_zone_ref).map_err(NowError::ProjectDateTime)
}

/// Get the current time in a given time zone, by name
///
/// # Errors
///
/// Possible errors include:
///
/// * The current [Unix time](https://en.wikipedia.org/w/index.php?title=Unix_time&oldid=1101650731)
///   could not be determined.
/// * The current Unix time could not be projected into the time zone.
///   Most likely the system time is off, or you are a time traveler trying run this code a few billion years in the future or past.
/// * The time zone is not a valid [IANA time zone](https://www.iana.org/time-zones).
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), tzdb::now::NowError> {
/// // What is the time in Berlin?
/// let now = tzdb::now::in_named("Europe/Berlin")?;
/// # Ok(()) }
/// ```
///
/// In most cases you will want to default to a specified time zone if the time zone was not found.
/// Then use e.g.
///
/// ```rust
/// # fn main() -> Result<(), tzdb::now::NowError> {
/// let now = tzdb::now::in_named_or(tzdb::time_zone::GMT, "Some/City")?;
/// # Ok(()) }
/// ```
///
/// # See also:
///
/// * [`local()`] / [`local_or()`]
/// * `in_named()` / [`in_named_or()`]
/// * [`in_tz()`]
pub fn in_named(tz: impl AsRef<[u8]>) -> Result<DateTime, NowError> {
    in_tz(crate::tz_by_name(tz).ok_or(NowError::UnknownTimezone)?)
}

/// Get the current time in a given time zone, by name, or default to some static time zone
///
/// # Errors
///
/// Possible errors include:
///
/// * The current [Unix time](https://en.wikipedia.org/w/index.php?title=Unix_time&oldid=1101650731)
///   could not be determined.
/// * The current Unix time could not be projected into the time zone.
///   Most likely the system time is off, or you are a time traveler trying run this code a few billion years in the future or past.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), tzdb::now::NowError> {
/// // What is the time in Some City?
/// let now = tzdb::now::in_named_or(tzdb::time_zone::GMT, "Some/City")?;
/// # Ok(()) }
/// ```
///
/// # See also:
///
/// * [`local()`] / [`local_or()`]
/// * [`in_named()`] / `in_named_or()`
/// * [`in_tz()`]
pub fn in_named_or(default: TimeZoneRef<'_>, tz: impl AsRef<[u8]>) -> Result<DateTime, NowError> {
    in_tz(crate::tz_by_name(tz).unwrap_or(default))
}
