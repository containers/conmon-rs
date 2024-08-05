//! Functions used for parsing a TZ string.

use crate::error::{TzError, TzStringError};
use crate::timezone::*;
use crate::utils::*;

use std::num::ParseIntError;
use std::str::{self, FromStr};

/// Parse integer from a slice of bytes
fn parse_int<T: FromStr<Err = ParseIntError>>(bytes: &[u8]) -> Result<T, TzStringError> {
    Ok(str::from_utf8(bytes)?.parse()?)
}

/// Parse time zone designation
fn parse_time_zone_designation<'a>(cursor: &mut Cursor<'a>) -> Result<&'a [u8], TzStringError> {
    let unquoted = if cursor.remaining().first() == Some(&b'<') {
        cursor.read_exact(1)?;
        let unquoted = cursor.read_until(|&x| x == b'>')?;
        cursor.read_exact(1)?;
        unquoted
    } else {
        cursor.read_while(u8::is_ascii_alphabetic)?
    };

    Ok(unquoted)
}

/// Parse hours, minutes and seconds
fn parse_hhmmss(cursor: &mut Cursor) -> Result<(i32, i32, i32), TzStringError> {
    let hour = parse_int(cursor.read_while(u8::is_ascii_digit)?)?;

    let mut minute = 0;
    let mut second = 0;

    if cursor.read_optional_tag(b":")? {
        minute = parse_int(cursor.read_while(u8::is_ascii_digit)?)?;

        if cursor.read_optional_tag(b":")? {
            second = parse_int(cursor.read_while(u8::is_ascii_digit)?)?;
        }
    }

    Ok((hour, minute, second))
}

/// Parse signed hours, minutes and seconds
fn parse_signed_hhmmss(cursor: &mut Cursor) -> Result<(i32, i32, i32, i32), TzStringError> {
    let mut sign = 1;
    if let Some(&c @ b'+') | Some(&c @ b'-') = cursor.remaining().first() {
        cursor.read_exact(1)?;
        if c == b'-' {
            sign = -1;
        }
    }

    let (hour, minute, second) = parse_hhmmss(cursor)?;
    Ok((sign, hour, minute, second))
}

/// Parse time zone offset
fn parse_offset(cursor: &mut Cursor) -> Result<i32, TzStringError> {
    let (sign, hour, minute, second) = parse_signed_hhmmss(cursor)?;

    if !(0..=24).contains(&hour) {
        return Err(TzStringError::InvalidTzString("invalid offset hour"));
    }
    if !(0..=59).contains(&minute) {
        return Err(TzStringError::InvalidTzString("invalid offset minute"));
    }
    if !(0..=59).contains(&second) {
        return Err(TzStringError::InvalidTzString("invalid offset second"));
    }

    Ok(sign * (hour * 3600 + minute * 60 + second))
}

/// Parse transition rule day
fn parse_rule_day(cursor: &mut Cursor) -> Result<RuleDay, TzError> {
    match cursor.remaining().first() {
        Some(b'J') => {
            cursor.read_exact(1)?;
            Ok(RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(parse_int(cursor.read_while(u8::is_ascii_digit)?)?)?))
        }
        Some(b'M') => {
            cursor.read_exact(1)?;

            let month = parse_int(cursor.read_while(u8::is_ascii_digit)?)?;
            cursor.read_tag(b".")?;
            let week = parse_int(cursor.read_while(u8::is_ascii_digit)?)?;
            cursor.read_tag(b".")?;
            let week_day = parse_int(cursor.read_while(u8::is_ascii_digit)?)?;

            Ok(RuleDay::MonthWeekDay(MonthWeekDay::new(month, week, week_day)?))
        }
        _ => Ok(RuleDay::Julian0WithLeap(Julian0WithLeap::new(parse_int(cursor.read_while(u8::is_ascii_digit)?)?)?)),
    }
}

/// Parse transition rule time
fn parse_rule_time(cursor: &mut Cursor) -> Result<i32, TzStringError> {
    let (hour, minute, second) = parse_hhmmss(cursor)?;

    if !(0..=24).contains(&hour) {
        return Err(TzStringError::InvalidTzString("invalid day time hour"));
    }
    if !(0..=59).contains(&minute) {
        return Err(TzStringError::InvalidTzString("invalid day time minute"));
    }
    if !(0..=59).contains(&second) {
        return Err(TzStringError::InvalidTzString("invalid day time second"));
    }

    Ok(hour * 3600 + minute * 60 + second)
}

/// Parse transition rule time with TZ string extensions
fn parse_rule_time_extended(cursor: &mut Cursor) -> Result<i32, TzStringError> {
    let (sign, hour, minute, second) = parse_signed_hhmmss(cursor)?;

    if !(-167..=167).contains(&hour) {
        return Err(TzStringError::InvalidTzString("invalid day time hour"));
    }
    if !(0..=59).contains(&minute) {
        return Err(TzStringError::InvalidTzString("invalid day time minute"));
    }
    if !(0..=59).contains(&second) {
        return Err(TzStringError::InvalidTzString("invalid day time second"));
    }

    Ok(sign * (hour * 3600 + minute * 60 + second))
}

/// Parse transition rule
fn parse_rule_block(cursor: &mut Cursor, use_string_extensions: bool) -> Result<(RuleDay, i32), TzError> {
    let date = parse_rule_day(cursor)?;

    let time = if cursor.read_optional_tag(b"/")? {
        if use_string_extensions {
            parse_rule_time_extended(cursor)?
        } else {
            parse_rule_time(cursor)?
        }
    } else {
        2 * 3600
    };

    Ok((date, time))
}

/// Parse a POSIX TZ string containing a time zone description, as described in [the POSIX documentation of the `TZ` environment variable](https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/V1_chap08.html).
///
/// TZ string extensions from [RFC 8536](https://datatracker.ietf.org/doc/html/rfc8536#section-3.3.1) may be used.
///
pub(crate) fn parse_posix_tz(tz_string: &[u8], use_string_extensions: bool) -> Result<TransitionRule, TzError> {
    let mut cursor = Cursor::new(tz_string);

    let std_time_zone = Some(parse_time_zone_designation(&mut cursor)?);
    let std_offset = parse_offset(&mut cursor)?;

    if cursor.is_empty() {
        return Ok(TransitionRule::Fixed(LocalTimeType::new(-std_offset, false, std_time_zone)?));
    }

    let dst_time_zone = Some(parse_time_zone_designation(&mut cursor)?);

    let dst_offset = match cursor.remaining().first() {
        Some(&b',') => std_offset - 3600,
        Some(_) => parse_offset(&mut cursor)?,
        None => return Err(TzStringError::UnsupportedTzString("DST start and end rules must be provided").into()),
    };

    if cursor.is_empty() {
        return Err(TzStringError::UnsupportedTzString("DST start and end rules must be provided").into());
    }

    cursor.read_tag(b",")?;
    let (dst_start, dst_start_time) = parse_rule_block(&mut cursor, use_string_extensions)?;

    cursor.read_tag(b",")?;
    let (dst_end, dst_end_time) = parse_rule_block(&mut cursor, use_string_extensions)?;

    if !cursor.is_empty() {
        return Err(TzStringError::InvalidTzString("remaining data after parsing TZ string").into());
    }

    Ok(TransitionRule::Alternate(AlternateTime::new(
        LocalTimeType::new(-std_offset, false, std_time_zone)?,
        LocalTimeType::new(-dst_offset, true, dst_time_zone)?,
        dst_start,
        dst_start_time,
        dst_end,
        dst_end_time,
    )?))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_no_dst() -> Result<(), TzError> {
        let tz_string = b"HST10";

        let transition_rule = parse_posix_tz(tz_string, false)?;
        let transition_rule_result = TransitionRule::Fixed(LocalTimeType::new(-36000, false, Some(b"HST"))?);

        assert_eq!(transition_rule, transition_rule_result);

        Ok(())
    }

    #[test]
    fn test_quoted() -> Result<(), TzError> {
        let tz_string = b"<-03>+3<+03>-3,J1,J365";

        let transition_rule = parse_posix_tz(tz_string, false)?;

        let transition_rule_result = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(-10800, false, Some(b"-03"))?,
            LocalTimeType::new(10800, true, Some(b"+03"))?,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(1)?),
            7200,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(365)?),
            7200,
        )?);

        assert_eq!(transition_rule, transition_rule_result);

        Ok(())
    }

    #[test]
    fn test_full() -> Result<(), TzError> {
        let tz_string = b"NZST-12:00:00NZDT-13:00:00,M10.1.0/02:00:00,M3.3.0/02:00:00";

        let transition_rule = parse_posix_tz(tz_string, false)?;

        let transition_rule_result = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(43200, false, Some(b"NZST"))?,
            LocalTimeType::new(46800, true, Some(b"NZDT"))?,
            RuleDay::MonthWeekDay(MonthWeekDay::new(10, 1, 0)?),
            7200,
            RuleDay::MonthWeekDay(MonthWeekDay::new(3, 3, 0)?),
            7200,
        )?);

        assert_eq!(transition_rule, transition_rule_result);

        Ok(())
    }

    #[test]
    fn test_negative_dst() -> Result<(), TzError> {
        let tz_string = b"IST-1GMT0,M10.5.0,M3.5.0/1";

        let transition_rule = parse_posix_tz(tz_string, false)?;

        let transition_rule_result = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(3600, false, Some(b"IST"))?,
            LocalTimeType::new(0, true, Some(b"GMT"))?,
            RuleDay::MonthWeekDay(MonthWeekDay::new(10, 5, 0)?),
            7200,
            RuleDay::MonthWeekDay(MonthWeekDay::new(3, 5, 0)?),
            3600,
        )?);

        assert_eq!(transition_rule, transition_rule_result);

        Ok(())
    }

    #[test]
    fn test_negative_hour() -> Result<(), TzError> {
        let tz_string = b"<-03>3<-02>,M3.5.0/-2,M10.5.0/-1";

        assert!(parse_posix_tz(tz_string, false).is_err());

        let transition_rule = parse_posix_tz(tz_string, true)?;

        let transition_rule_result = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(-10800, false, Some(b"-03"))?,
            LocalTimeType::new(-7200, true, Some(b"-02"))?,
            RuleDay::MonthWeekDay(MonthWeekDay::new(3, 5, 0)?),
            -7200,
            RuleDay::MonthWeekDay(MonthWeekDay::new(10, 5, 0)?),
            -3600,
        )?);

        assert_eq!(transition_rule, transition_rule_result);

        Ok(())
    }

    #[test]
    fn test_all_year_dst() -> Result<(), TzError> {
        let tz_string = b"EST5EDT,0/0,J365/25";

        assert!(parse_posix_tz(tz_string, false).is_err());

        let transition_rule = parse_posix_tz(tz_string, true)?;

        let transition_rule_result = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(-18000, false, Some(b"EST"))?,
            LocalTimeType::new(-14400, true, Some(b"EDT"))?,
            RuleDay::Julian0WithLeap(Julian0WithLeap::new(0)?),
            0,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(365)?),
            90000,
        )?);

        assert_eq!(transition_rule, transition_rule_result);

        Ok(())
    }

    #[test]
    fn test_min_dst_offset() -> Result<(), TzError> {
        let tz_string = b"STD24:59:59DST,J1,J365";

        let transition_rule = parse_posix_tz(tz_string, false)?;

        let transition_rule_result = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(-89999, false, Some(b"STD"))?,
            LocalTimeType::new(-86399, true, Some(b"DST"))?,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(1)?),
            7200,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(365)?),
            7200,
        )?);

        assert_eq!(transition_rule, transition_rule_result);

        Ok(())
    }

    #[test]
    fn test_max_dst_offset() -> Result<(), TzError> {
        let tz_string = b"STD-24:59:59DST,J1,J365";

        let transition_rule = parse_posix_tz(tz_string, false)?;

        let transition_rule_result = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(89999, false, Some(b"STD"))?,
            LocalTimeType::new(93599, true, Some(b"DST"))?,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(1)?),
            7200,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(365)?),
            7200,
        )?);

        assert_eq!(transition_rule, transition_rule_result);

        Ok(())
    }

    #[test]
    fn test_error() -> Result<(), TzError> {
        assert!(matches!(parse_posix_tz(b"IST-1GMT0", false), Err(TzError::TzStringError(TzStringError::UnsupportedTzString(_)))));
        assert!(matches!(parse_posix_tz(b"EET-2EEST", false), Err(TzError::TzStringError(TzStringError::UnsupportedTzString(_)))));

        Ok(())
    }
}
