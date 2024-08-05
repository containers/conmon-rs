//! Types related to a time zone extra transition rule.

use crate::constants::*;
use crate::timezone::*;

/// Informations needed for checking DST transition rules consistency, for a Julian day
#[derive(Debug, PartialEq, Eq)]
struct JulianDayCheckInfos {
    /// Offset in seconds from the start of a normal year
    start_normal_year_offset: i64,
    /// Offset in seconds from the end of a normal year
    end_normal_year_offset: i64,
    /// Offset in seconds from the start of a leap year
    start_leap_year_offset: i64,
    /// Offset in seconds from the end of a leap year
    end_leap_year_offset: i64,
}

/// Informations needed for checking DST transition rules consistency, for a day represented by a month, a month week and a week day
#[derive(Debug, PartialEq, Eq)]
struct MonthWeekDayCheckInfos {
    /// Possible offset range in seconds from the start of a normal year
    start_normal_year_offset_range: (i64, i64),
    /// Possible offset range in seconds from the end of a normal year
    end_normal_year_offset_range: (i64, i64),
    /// Possible offset range in seconds from the start of a leap year
    start_leap_year_offset_range: (i64, i64),
    /// Possible offset range in seconds from the end of a leap year
    end_leap_year_offset_range: (i64, i64),
}

/// Julian day in `[1, 365]`, without taking occasional February 29th into account, which is not referenceable
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct Julian1WithoutLeap(u16);

impl Julian1WithoutLeap {
    /// Construct a transition rule day represented by a Julian day in `[1, 365]`, without taking occasional February 29th into account, which is not referenceable
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn new(julian_day_1: u16) -> Result<Self, TransitionRuleError> {
        if !(1 <= julian_day_1 && julian_day_1 <= 365) {
            return Err(TransitionRuleError("invalid rule day julian day"));
        }

        Ok(Self(julian_day_1))
    }

    /// Returns inner value
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn get(&self) -> u16 {
        self.0
    }

    /// Compute transition date
    ///
    /// ## Outputs
    ///
    /// * `month`: Month in `[1, 12]`
    /// * `month_day`: Day of the month in `[1, 31]`
    ///
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    fn transition_date(&self) -> (usize, i64) {
        let year_day = self.0 as i64;

        let month = match binary_search_i64(&CUMUL_DAYS_IN_MONTHS_NORMAL_YEAR, year_day - 1) {
            Ok(x) => x + 1,
            Err(x) => x,
        };

        let month_day = year_day - CUMUL_DAYS_IN_MONTHS_NORMAL_YEAR[month - 1];

        (month, month_day)
    }

    /// Compute the informations needed for checking DST transition rules consistency
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    fn compute_check_infos(&self, utc_day_time: i64) -> JulianDayCheckInfos {
        let start_normal_year_offset = (self.0 as i64 - 1) * SECONDS_PER_DAY + utc_day_time;
        let start_leap_year_offset = if self.0 <= 59 { start_normal_year_offset } else { start_normal_year_offset + SECONDS_PER_DAY };

        JulianDayCheckInfos {
            start_normal_year_offset,
            end_normal_year_offset: start_normal_year_offset - SECONDS_PER_NORMAL_YEAR,
            start_leap_year_offset,
            end_leap_year_offset: start_leap_year_offset - SECONDS_PER_LEAP_YEAR,
        }
    }
}

/// Zero-based Julian day in `[0, 365]`, taking occasional February 29th into account and allowing December 32nd
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct Julian0WithLeap(u16);

impl Julian0WithLeap {
    /// Construct a transition rule day represented by a zero-based Julian day in `[0, 365]`, taking occasional February 29th into account and allowing December 32nd
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn new(julian_day_0: u16) -> Result<Self, TransitionRuleError> {
        if julian_day_0 > 365 {
            return Err(TransitionRuleError("invalid rule day julian day"));
        }

        Ok(Self(julian_day_0))
    }

    /// Returns inner value
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn get(&self) -> u16 {
        self.0
    }

    /// Compute transition date.
    ///
    /// On a non-leap year, a value of `365` corresponds to December 32nd (which is January 1st of the next year).
    ///
    /// ## Outputs
    ///
    /// * `month`: Month in `[1, 12]`
    /// * `month_day`: Day of the month in `[1, 32]`
    ///
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    fn transition_date(&self, leap_year: bool) -> (usize, i64) {
        let cumul_day_in_months = if leap_year { &CUMUL_DAYS_IN_MONTHS_LEAP_YEAR } else { &CUMUL_DAYS_IN_MONTHS_NORMAL_YEAR };

        let year_day = self.0 as i64;

        let month = match binary_search_i64(cumul_day_in_months, year_day) {
            Ok(x) => x + 1,
            Err(x) => x,
        };

        let month_day = 1 + year_day - cumul_day_in_months[month - 1];

        (month, month_day)
    }

    /// Compute the informations needed for checking DST transition rules consistency
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    fn compute_check_infos(&self, utc_day_time: i64) -> JulianDayCheckInfos {
        let start_year_offset = self.0 as i64 * SECONDS_PER_DAY + utc_day_time;

        JulianDayCheckInfos {
            start_normal_year_offset: start_year_offset,
            end_normal_year_offset: start_year_offset - SECONDS_PER_NORMAL_YEAR,
            start_leap_year_offset: start_year_offset,
            end_leap_year_offset: start_year_offset - SECONDS_PER_LEAP_YEAR,
        }
    }
}

/// Day represented by a month, a month week and a week day
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct MonthWeekDay {
    /// Month in `[1, 12]`
    month: u8,
    /// Week of the month in `[1, 5]`, with `5` representing the last week of the month
    week: u8,
    /// Day of the week in `[0, 6]` from Sunday
    week_day: u8,
}

impl MonthWeekDay {
    /// Construct a transition rule day represented by a month, a month week and a week day
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn new(month: u8, week: u8, week_day: u8) -> Result<Self, TransitionRuleError> {
        if !(1 <= month && month <= 12) {
            return Err(TransitionRuleError("invalid rule day month"));
        }

        if !(1 <= week && week <= 5) {
            return Err(TransitionRuleError("invalid rule day week"));
        }

        if week_day > 6 {
            return Err(TransitionRuleError("invalid rule day week day"));
        }

        Ok(Self { month, week, week_day })
    }

    /// Returns month in `[1, 12]`
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn month(&self) -> u8 {
        self.month
    }

    /// Returns week of the month in `[1, 5]`, with `5` representing the last week of the month
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn week(&self) -> u8 {
        self.week
    }

    /// Returns day of the week in `[0, 6]` from Sunday
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn week_day(&self) -> u8 {
        self.week_day
    }

    /// Compute transition date on a specific year
    ///
    /// ## Outputs
    ///
    /// * `month`: Month in `[1, 12]`
    /// * `month_day`: Day of the month in `[1, 31]`
    ///
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    fn transition_date(&self, year: i32) -> (usize, i64) {
        let month = self.month as usize;
        let week = self.week as i64;
        let week_day = self.week_day as i64;

        let mut days_in_month = DAYS_IN_MONTHS_NORMAL_YEAR[month - 1];
        if month == 2 {
            days_in_month += is_leap_year(year) as i64;
        }

        let week_day_of_first_month_day = (4 + days_since_unix_epoch(year, month, 1)).rem_euclid(DAYS_PER_WEEK);
        let first_week_day_occurence_in_month = 1 + (week_day as i64 - week_day_of_first_month_day).rem_euclid(DAYS_PER_WEEK);

        let mut month_day = first_week_day_occurence_in_month + (week as i64 - 1) * DAYS_PER_WEEK;
        if month_day > days_in_month {
            month_day -= DAYS_PER_WEEK
        }

        (month, month_day)
    }

    /// Compute the informations needed for checking DST transition rules consistency
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    fn compute_check_infos(&self, utc_day_time: i64) -> MonthWeekDayCheckInfos {
        let month = self.month as usize;
        let week = self.week as i64;

        let (normal_year_month_day_range, leap_year_month_day_range) = {
            if week == 5 {
                let normal_year_days_in_month = DAYS_IN_MONTHS_NORMAL_YEAR[month - 1];
                let leap_year_days_in_month = if month == 2 { normal_year_days_in_month + 1 } else { normal_year_days_in_month };

                let normal_year_month_day_range = (normal_year_days_in_month - 6, normal_year_days_in_month);
                let leap_year_month_day_range = (leap_year_days_in_month - 6, leap_year_days_in_month);

                (normal_year_month_day_range, leap_year_month_day_range)
            } else {
                let month_day_range = (week * DAYS_PER_WEEK - 6, week * DAYS_PER_WEEK);
                (month_day_range, month_day_range)
            }
        };

        let start_normal_year_offset_range = (
            (CUMUL_DAYS_IN_MONTHS_NORMAL_YEAR[month - 1] + normal_year_month_day_range.0 - 1) * SECONDS_PER_DAY + utc_day_time,
            (CUMUL_DAYS_IN_MONTHS_NORMAL_YEAR[month - 1] + normal_year_month_day_range.1 - 1) * SECONDS_PER_DAY + utc_day_time,
        );

        let start_leap_year_offset_range = (
            (CUMUL_DAYS_IN_MONTHS_LEAP_YEAR[month - 1] + leap_year_month_day_range.0 - 1) * SECONDS_PER_DAY + utc_day_time,
            (CUMUL_DAYS_IN_MONTHS_LEAP_YEAR[month - 1] + leap_year_month_day_range.1 - 1) * SECONDS_PER_DAY + utc_day_time,
        );

        MonthWeekDayCheckInfos {
            start_normal_year_offset_range,
            end_normal_year_offset_range: (
                start_normal_year_offset_range.0 - SECONDS_PER_NORMAL_YEAR,
                start_normal_year_offset_range.1 - SECONDS_PER_NORMAL_YEAR,
            ),
            start_leap_year_offset_range,
            end_leap_year_offset_range: (start_leap_year_offset_range.0 - SECONDS_PER_LEAP_YEAR, start_leap_year_offset_range.1 - SECONDS_PER_LEAP_YEAR),
        }
    }
}

/// Transition rule day
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RuleDay {
    /// Julian day in `[1, 365]`, without taking occasional February 29th into account, which is not referenceable
    Julian1WithoutLeap(Julian1WithoutLeap),
    /// Zero-based Julian day in `[0, 365]`, taking occasional February 29th into account and allowing December 32nd
    Julian0WithLeap(Julian0WithLeap),
    /// Day represented by a month, a month week and a week day
    MonthWeekDay(MonthWeekDay),
}

impl RuleDay {
    /// Compute transition date for the provided year.
    ///
    /// The December 32nd date is possible, which corresponds to January 1st of the next year.
    ///
    /// ## Outputs
    ///
    /// * `month`: Month in `[1, 12]`
    /// * `month_day`: Day of the month in `[1, 32]`
    ///
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    fn transition_date(&self, year: i32) -> (usize, i64) {
        match self {
            Self::Julian1WithoutLeap(rule_day) => rule_day.transition_date(),
            Self::Julian0WithLeap(rule_day) => rule_day.transition_date(is_leap_year(year)),
            Self::MonthWeekDay(rule_day) => rule_day.transition_date(year),
        }
    }

    /// Returns the UTC Unix time in seconds associated to the transition date for the provided year
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub(crate) fn unix_time(&self, year: i32, day_time_in_utc: i64) -> i64 {
        let (month, month_day) = self.transition_date(year);
        days_since_unix_epoch(year, month, month_day) * SECONDS_PER_DAY + day_time_in_utc
    }
}

/// Transition rule representing alternate local time types
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct AlternateTime {
    /// Local time type for standard time
    std: LocalTimeType,
    /// Local time type for Daylight Saving Time
    dst: LocalTimeType,
    /// Start day of Daylight Saving Time
    dst_start: RuleDay,
    /// Local start day time of Daylight Saving Time, in seconds
    dst_start_time: i32,
    /// End day of Daylight Saving Time
    dst_end: RuleDay,
    /// Local end day time of Daylight Saving Time, in seconds
    dst_end_time: i32,
}

impl AlternateTime {
    /// Construct a transition rule representing alternate local time types
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn new(
        std: LocalTimeType,
        dst: LocalTimeType,
        dst_start: RuleDay,
        dst_start_time: i32,
        dst_end: RuleDay,
        dst_end_time: i32,
    ) -> Result<Self, TransitionRuleError> {
        let std_ut_offset = std.ut_offset as i64;
        let dst_ut_offset = dst.ut_offset as i64;

        // Limit UTC offset to POSIX-required range
        if !(-25 * SECONDS_PER_HOUR < std_ut_offset && std_ut_offset < 26 * SECONDS_PER_HOUR) {
            return Err(TransitionRuleError("invalid standard time UTC offset"));
        }

        if !(-25 * SECONDS_PER_HOUR < dst_ut_offset && dst_ut_offset < 26 * SECONDS_PER_HOUR) {
            return Err(TransitionRuleError("invalid Daylight Saving Time UTC offset"));
        }

        // Overflow is not possible
        if !((dst_start_time as i64).abs() < SECONDS_PER_WEEK && (dst_end_time as i64).abs() < SECONDS_PER_WEEK) {
            return Err(TransitionRuleError("invalid DST start or end time"));
        }

        // Check DST transition rules consistency
        if !check_dst_transition_rules_consistency(&std, &dst, dst_start, dst_start_time, dst_end, dst_end_time) {
            return Err(TransitionRuleError("DST transition rules are not consistent from one year to another"));
        }

        Ok(Self { std, dst, dst_start, dst_start_time, dst_end, dst_end_time })
    }

    /// Returns local time type for standard time
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn std(&self) -> &LocalTimeType {
        &self.std
    }

    /// Returns local time type for Daylight Saving Time
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn dst(&self) -> &LocalTimeType {
        &self.dst
    }

    /// Returns start day of Daylight Saving Time
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn dst_start(&self) -> &RuleDay {
        &self.dst_start
    }

    /// Returns local start day time of Daylight Saving Time, in seconds
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn dst_start_time(&self) -> i32 {
        self.dst_start_time
    }

    /// Returns end day of Daylight Saving Time
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn dst_end(&self) -> &RuleDay {
        &self.dst_end
    }

    /// Returns local end day time of Daylight Saving Time, in seconds
    #[inline]
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub fn dst_end_time(&self) -> i32 {
        self.dst_end_time
    }

    /// Find the local time type associated to the alternate transition rule at the specified Unix time in seconds
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    fn find_local_time_type(&self, unix_time: i64) -> Result<&LocalTimeType, OutOfRangeError> {
        // Overflow is not possible
        let dst_start_time_in_utc = self.dst_start_time as i64 - self.std.ut_offset as i64;
        let dst_end_time_in_utc = self.dst_end_time as i64 - self.dst.ut_offset as i64;

        let current_year = match UtcDateTime::from_timespec(unix_time, 0) {
            Ok(utc_date_time) => utc_date_time.year(),
            Err(error) => return Err(error),
        };

        // Check if the current year is valid for the following computations
        if !(i32::MIN + 2 <= current_year && current_year <= i32::MAX - 2) {
            return Err(OutOfRangeError("out of range date time"));
        }

        let current_year_dst_start_unix_time = self.dst_start.unix_time(current_year, dst_start_time_in_utc);
        let current_year_dst_end_unix_time = self.dst_end.unix_time(current_year, dst_end_time_in_utc);

        // Check DST start/end Unix times for previous/current/next years to support for transition day times outside of [0h, 24h] range.
        // This is sufficient since the absolute value of DST start/end time in UTC is less than 2 weeks.
        // Moreover, inconsistent DST transition rules are not allowed, so there won't be additional transitions at the year boundary.
        let is_dst = match cmp(current_year_dst_start_unix_time, current_year_dst_end_unix_time) {
            Ordering::Less | Ordering::Equal => {
                if unix_time < current_year_dst_start_unix_time {
                    let previous_year_dst_end_unix_time = self.dst_end.unix_time(current_year - 1, dst_end_time_in_utc);
                    if unix_time < previous_year_dst_end_unix_time {
                        let previous_year_dst_start_unix_time = self.dst_start.unix_time(current_year - 1, dst_start_time_in_utc);
                        previous_year_dst_start_unix_time <= unix_time
                    } else {
                        false
                    }
                } else if unix_time < current_year_dst_end_unix_time {
                    true
                } else {
                    let next_year_dst_start_unix_time = self.dst_start.unix_time(current_year + 1, dst_start_time_in_utc);
                    if next_year_dst_start_unix_time <= unix_time {
                        let next_year_dst_end_unix_time = self.dst_end.unix_time(current_year + 1, dst_end_time_in_utc);
                        unix_time < next_year_dst_end_unix_time
                    } else {
                        false
                    }
                }
            }
            Ordering::Greater => {
                if unix_time < current_year_dst_end_unix_time {
                    let previous_year_dst_start_unix_time = self.dst_start.unix_time(current_year - 1, dst_start_time_in_utc);
                    if unix_time < previous_year_dst_start_unix_time {
                        let previous_year_dst_end_unix_time = self.dst_end.unix_time(current_year - 1, dst_end_time_in_utc);
                        unix_time < previous_year_dst_end_unix_time
                    } else {
                        true
                    }
                } else if unix_time < current_year_dst_start_unix_time {
                    false
                } else {
                    let next_year_dst_end_unix_time = self.dst_end.unix_time(current_year + 1, dst_end_time_in_utc);
                    if next_year_dst_end_unix_time <= unix_time {
                        let next_year_dst_start_unix_time = self.dst_start.unix_time(current_year + 1, dst_start_time_in_utc);
                        next_year_dst_start_unix_time <= unix_time
                    } else {
                        true
                    }
                }
            }
        };

        if is_dst {
            Ok(&self.dst)
        } else {
            Ok(&self.std)
        }
    }
}

/// Transition rule
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TransitionRule {
    /// Fixed local time type
    Fixed(LocalTimeType),
    /// Alternate local time types
    Alternate(AlternateTime),
}

impl TransitionRule {
    /// Find the local time type associated to the transition rule at the specified Unix time in seconds
    #[cfg_attr(feature = "const", const_fn::const_fn)]
    pub(super) fn find_local_time_type(&self, unix_time: i64) -> Result<&LocalTimeType, OutOfRangeError> {
        match self {
            Self::Fixed(local_time_type) => Ok(local_time_type),
            Self::Alternate(alternate_time) => alternate_time.find_local_time_type(unix_time),
        }
    }
}

/// Check DST transition rules consistency, which ensures that the DST start and end time are always in the same order.
///
/// This prevents from having an additional transition at the year boundary, when the order of DST start and end time is different on consecutive years.
///
#[cfg_attr(feature = "const", const_fn::const_fn)]
fn check_dst_transition_rules_consistency(
    std: &LocalTimeType,
    dst: &LocalTimeType,
    dst_start: RuleDay,
    dst_start_time: i32,
    dst_end: RuleDay,
    dst_end_time: i32,
) -> bool {
    // Overflow is not possible
    let dst_start_time_in_utc = dst_start_time as i64 - std.ut_offset as i64;
    let dst_end_time_in_utc = dst_end_time as i64 - dst.ut_offset as i64;

    match (dst_start, dst_end) {
        (RuleDay::Julian1WithoutLeap(start_day), RuleDay::Julian1WithoutLeap(end_day)) => {
            check_two_julian_days(start_day.compute_check_infos(dst_start_time_in_utc), end_day.compute_check_infos(dst_end_time_in_utc))
        }
        (RuleDay::Julian1WithoutLeap(start_day), RuleDay::Julian0WithLeap(end_day)) => {
            check_two_julian_days(start_day.compute_check_infos(dst_start_time_in_utc), end_day.compute_check_infos(dst_end_time_in_utc))
        }
        (RuleDay::Julian0WithLeap(start_day), RuleDay::Julian1WithoutLeap(end_day)) => {
            check_two_julian_days(start_day.compute_check_infos(dst_start_time_in_utc), end_day.compute_check_infos(dst_end_time_in_utc))
        }
        (RuleDay::Julian0WithLeap(start_day), RuleDay::Julian0WithLeap(end_day)) => {
            check_two_julian_days(start_day.compute_check_infos(dst_start_time_in_utc), end_day.compute_check_infos(dst_end_time_in_utc))
        }
        (RuleDay::Julian1WithoutLeap(start_day), RuleDay::MonthWeekDay(end_day)) => {
            check_month_week_day_and_julian_day(end_day.compute_check_infos(dst_end_time_in_utc), start_day.compute_check_infos(dst_start_time_in_utc))
        }
        (RuleDay::Julian0WithLeap(start_day), RuleDay::MonthWeekDay(end_day)) => {
            check_month_week_day_and_julian_day(end_day.compute_check_infos(dst_end_time_in_utc), start_day.compute_check_infos(dst_start_time_in_utc))
        }
        (RuleDay::MonthWeekDay(start_day), RuleDay::Julian1WithoutLeap(end_day)) => {
            check_month_week_day_and_julian_day(start_day.compute_check_infos(dst_start_time_in_utc), end_day.compute_check_infos(dst_end_time_in_utc))
        }
        (RuleDay::MonthWeekDay(start_day), RuleDay::Julian0WithLeap(end_day)) => {
            check_month_week_day_and_julian_day(start_day.compute_check_infos(dst_start_time_in_utc), end_day.compute_check_infos(dst_end_time_in_utc))
        }
        (RuleDay::MonthWeekDay(start_day), RuleDay::MonthWeekDay(end_day)) => {
            check_two_month_week_days(start_day, dst_start_time_in_utc, end_day, dst_end_time_in_utc)
        }
    }
}

/// Check DST transition rules consistency for two Julian days
#[cfg_attr(feature = "const", const_fn::const_fn)]
fn check_two_julian_days(check_infos_1: JulianDayCheckInfos, check_infos_2: JulianDayCheckInfos) -> bool {
    // Check in same year
    let (before, after) = if check_infos_1.start_normal_year_offset <= check_infos_2.start_normal_year_offset
        && check_infos_1.start_leap_year_offset <= check_infos_2.start_leap_year_offset
    {
        (&check_infos_1, &check_infos_2)
    } else if check_infos_2.start_normal_year_offset <= check_infos_1.start_normal_year_offset
        && check_infos_2.start_leap_year_offset <= check_infos_1.start_leap_year_offset
    {
        (&check_infos_2, &check_infos_1)
    } else {
        return false;
    };

    // Check in consecutive years
    if after.end_normal_year_offset <= before.start_normal_year_offset
        && after.end_normal_year_offset <= before.start_leap_year_offset
        && after.end_leap_year_offset <= before.start_normal_year_offset
    {
        return true;
    }

    if before.start_normal_year_offset <= after.end_normal_year_offset
        && before.start_leap_year_offset <= after.end_normal_year_offset
        && before.start_normal_year_offset <= after.end_leap_year_offset
    {
        return true;
    }

    false
}

/// Check DST transition rules consistency for a Julian day and a day represented by a month, a month week and a week day
#[cfg_attr(feature = "const", const_fn::const_fn)]
fn check_month_week_day_and_julian_day(check_infos_1: MonthWeekDayCheckInfos, check_infos_2: JulianDayCheckInfos) -> bool {
    // Check in same year, then in consecutive years
    if check_infos_2.start_normal_year_offset <= check_infos_1.start_normal_year_offset_range.0
        && check_infos_2.start_leap_year_offset <= check_infos_1.start_leap_year_offset_range.0
    {
        let (before, after) = (&check_infos_2, &check_infos_1);

        if after.end_normal_year_offset_range.1 <= before.start_normal_year_offset
            && after.end_normal_year_offset_range.1 <= before.start_leap_year_offset
            && after.end_leap_year_offset_range.1 <= before.start_normal_year_offset
        {
            return true;
        };

        if before.start_normal_year_offset <= after.end_normal_year_offset_range.0
            && before.start_leap_year_offset <= after.end_normal_year_offset_range.0
            && before.start_normal_year_offset <= after.end_leap_year_offset_range.0
        {
            return true;
        };

        return false;
    }

    if check_infos_1.start_normal_year_offset_range.1 <= check_infos_2.start_normal_year_offset
        && check_infos_1.start_leap_year_offset_range.1 <= check_infos_2.start_leap_year_offset
    {
        let (before, after) = (&check_infos_1, &check_infos_2);

        if after.end_normal_year_offset <= before.start_normal_year_offset_range.0
            && after.end_normal_year_offset <= before.start_leap_year_offset_range.0
            && after.end_leap_year_offset <= before.start_normal_year_offset_range.0
        {
            return true;
        }

        if before.start_normal_year_offset_range.1 <= after.end_normal_year_offset
            && before.start_leap_year_offset_range.1 <= after.end_normal_year_offset
            && before.start_normal_year_offset_range.1 <= after.end_leap_year_offset
        {
            return true;
        }

        return false;
    }

    false
}

/// Check DST transition rules consistency for two days represented by a month, a month week and a week day
#[cfg_attr(feature = "const", const_fn::const_fn)]
fn check_two_month_week_days(month_week_day_1: MonthWeekDay, utc_day_time_1: i64, month_week_day_2: MonthWeekDay, utc_day_time_2: i64) -> bool {
    // Sort rule days
    let (month_week_day_before, utc_day_time_before, month_week_day_after, utc_day_time_after) = {
        let rem = (month_week_day_2.month as i64 - month_week_day_1.month as i64).rem_euclid(MONTHS_PER_YEAR);

        if rem == 0 {
            if month_week_day_1.week <= month_week_day_2.week {
                (month_week_day_1, utc_day_time_1, month_week_day_2, utc_day_time_2)
            } else {
                (month_week_day_2, utc_day_time_2, month_week_day_1, utc_day_time_1)
            }
        } else if rem == 1 {
            (month_week_day_1, utc_day_time_1, month_week_day_2, utc_day_time_2)
        } else if rem == MONTHS_PER_YEAR - 1 {
            (month_week_day_2, utc_day_time_2, month_week_day_1, utc_day_time_1)
        } else {
            // Months are not equal or consecutive, so rule days are separated by more than 3 weeks and cannot swap their order
            return true;
        }
    };

    let month_before = month_week_day_before.month as usize;
    let week_before = month_week_day_before.week as i64;
    let week_day_before = month_week_day_before.week_day as i64;

    let month_after = month_week_day_after.month as usize;
    let week_after = month_week_day_after.week as i64;
    let week_day_after = month_week_day_after.week_day as i64;

    let (diff_days_min, diff_days_max) = if week_day_before == week_day_after {
        // Rule days are separated by a whole number of weeks
        let (diff_week_min, diff_week_max) = match (week_before, week_after) {
            // All months have more than 29 days on a leap year, so the 5th week is non-empty
            (1..=4, 5) if month_before == month_after => (4 - week_before, 5 - week_before),
            (1..=4, 1..=4) if month_before != month_after => (4 - week_before + week_after, 5 - week_before + week_after),
            _ => return true, // rule days are synchronized or separated by more than 3 weeks
        };

        (diff_week_min * DAYS_PER_WEEK, diff_week_max * DAYS_PER_WEEK)
    } else {
        // week_day_before != week_day_after
        let n = (week_day_after - week_day_before).rem_euclid(DAYS_PER_WEEK); // n >= 1

        if month_before == month_after {
            match (week_before, week_after) {
                (5, 5) => (n - DAYS_PER_WEEK, n),
                (1..=4, 1..=4) => (n + DAYS_PER_WEEK * (week_after - week_before - 1), n + DAYS_PER_WEEK * (week_after - week_before)),
                (1..=4, 5) => {
                    // For February month:
                    //   * On a normal year, we have n > (days_in_month % DAYS_PER_WEEK).
                    //   * On a leap year, we have n >= (days_in_month % DAYS_PER_WEEK).
                    //
                    // Since we want to check all possible years at the same time, checking only non-leap year is enough.
                    let days_in_month = DAYS_IN_MONTHS_NORMAL_YEAR[month_before - 1];

                    match cmp(n, days_in_month % DAYS_PER_WEEK) {
                        Ordering::Less => (n + DAYS_PER_WEEK * (4 - week_before), n + DAYS_PER_WEEK * (5 - week_before)),
                        Ordering::Equal => return true, // rule days are synchronized
                        Ordering::Greater => (n + DAYS_PER_WEEK * (3 - week_before), n + DAYS_PER_WEEK * (4 - week_before)),
                    }
                }
                _ => const_panic!(), // unreachable
            }
        } else {
            // month_before != month_after
            match (week_before, week_after) {
                (1..=4, 1..=4) => {
                    // Same as above
                    let days_in_month = DAYS_IN_MONTHS_NORMAL_YEAR[month_before - 1];

                    match cmp(n, days_in_month % DAYS_PER_WEEK) {
                        Ordering::Less => (n + DAYS_PER_WEEK * (4 - week_before + week_after), n + DAYS_PER_WEEK * (5 - week_before + week_after)),
                        Ordering::Equal => return true, // rule days are synchronized
                        Ordering::Greater => (n + DAYS_PER_WEEK * (3 - week_before + week_after), n + DAYS_PER_WEEK * (4 - week_before + week_after)),
                    }
                }
                (5, 1..=4) => (n + DAYS_PER_WEEK * (week_after - 1), n + DAYS_PER_WEEK * week_after),
                _ => return true, // rule days are separated by more than 3 weeks
            }
        }
    };

    let diff_days_seconds_min = diff_days_min * SECONDS_PER_DAY;
    let diff_days_seconds_max = diff_days_max * SECONDS_PER_DAY;

    // Check possible order swap of rule days
    utc_day_time_before <= diff_days_seconds_min + utc_day_time_after || diff_days_seconds_max + utc_day_time_after <= utc_day_time_before
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Result;

    #[test]
    fn test_compute_check_infos() -> Result<()> {
        let check_julian = |check_infos: JulianDayCheckInfos, start_normal, end_normal, start_leap, end_leap| {
            assert_eq!(check_infos.start_normal_year_offset, start_normal);
            assert_eq!(check_infos.end_normal_year_offset, end_normal);
            assert_eq!(check_infos.start_leap_year_offset, start_leap);
            assert_eq!(check_infos.end_leap_year_offset, end_leap);
        };

        let check_mwd = |check_infos: MonthWeekDayCheckInfos, start_normal, end_normal, start_leap, end_leap| {
            assert_eq!(check_infos.start_normal_year_offset_range, start_normal);
            assert_eq!(check_infos.end_normal_year_offset_range, end_normal);
            assert_eq!(check_infos.start_leap_year_offset_range, start_leap);
            assert_eq!(check_infos.end_leap_year_offset_range, end_leap);
        };

        check_julian(Julian1WithoutLeap::new(1)?.compute_check_infos(1), 1, -31535999, 1, -31622399);
        check_julian(Julian1WithoutLeap::new(365)?.compute_check_infos(1), 31449601, -86399, 31536001, -86399);

        check_julian(Julian0WithLeap::new(0)?.compute_check_infos(1), 1, -31535999, 1, -31622399);
        check_julian(Julian0WithLeap::new(365)?.compute_check_infos(1), 31536001, 1, 31536001, -86399);

        check_mwd(MonthWeekDay::new(1, 1, 0)?.compute_check_infos(1), (1, 518401), (-31535999, -31017599), (1, 518401), (-31622399, -31103999));
        check_mwd(MonthWeekDay::new(1, 5, 0)?.compute_check_infos(1), (2073601, 2592001), (-29462399, -28943999), (2073601, 2592001), (-29548799, -29030399));
        check_mwd(MonthWeekDay::new(2, 4, 0)?.compute_check_infos(1), (4492801, 5011201), (-27043199, -26524799), (4492801, 5011201), (-27129599, -26611199));
        check_mwd(MonthWeekDay::new(2, 5, 0)?.compute_check_infos(1), (4492801, 5011201), (-27043199, -26524799), (4579201, 5097601), (-27043199, -26524799));
        check_mwd(MonthWeekDay::new(3, 1, 0)?.compute_check_infos(1), (5097601, 5616001), (-26438399, -25919999), (5184001, 5702401), (-26438399, -25919999));
        check_mwd(MonthWeekDay::new(3, 5, 0)?.compute_check_infos(1), (7171201, 7689601), (-24364799, -23846399), (7257601, 7776001), (-24364799, -23846399));
        check_mwd(MonthWeekDay::new(12, 5, 0)?.compute_check_infos(1), (30931201, 31449601), (-604799, -86399), (31017601, 31536001), (-604799, -86399));

        Ok(())
    }

    #[test]
    fn test_check_dst_transition_rules_consistency() -> Result<()> {
        let utc = LocalTimeType::utc();

        let julian_1 = |year_day| Result::Ok(RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(year_day)?));
        let julian_0 = |year_day| Result::Ok(RuleDay::Julian0WithLeap(Julian0WithLeap::new(year_day)?));
        let mwd = |month, week, week_day| Result::Ok(RuleDay::MonthWeekDay(MonthWeekDay::new(month, week, week_day)?));

        let check = |dst_start, dst_start_time, dst_end, dst_end_time| {
            let check_1 = check_dst_transition_rules_consistency(&utc, &utc, dst_start, dst_start_time, dst_end, dst_end_time);
            let check_2 = check_dst_transition_rules_consistency(&utc, &utc, dst_end, dst_end_time, dst_start, dst_start_time);
            assert_eq!(check_1, check_2);

            check_1
        };

        let check_all = |dst_start, dst_start_times: &[i32], dst_end, dst_end_time, results: &[bool]| {
            assert_eq!(dst_start_times.len(), results.len());

            for (&dst_start_time, &result) in dst_start_times.iter().zip(results) {
                assert_eq!(check(dst_start, dst_start_time, dst_end, dst_end_time), result);
            }
        };

        const DAY_1: i32 = 86400;
        const DAY_2: i32 = 2 * DAY_1;
        const DAY_3: i32 = 3 * DAY_1;
        const DAY_4: i32 = 4 * DAY_1;
        const DAY_5: i32 = 5 * DAY_1;
        const DAY_6: i32 = 6 * DAY_1;

        check_all(julian_1(59)?, &[-1, 0, 1], julian_1(60)?, -DAY_1, &[true, true, false]);
        check_all(julian_1(365)?, &[-1, 0, 1], julian_1(1)?, -DAY_1, &[true, true, true]);

        check_all(julian_0(58)?, &[-1, 0, 1], julian_0(59)?, -DAY_1, &[true, true, true]);
        check_all(julian_0(364)?, &[-1, 0, 1], julian_0(0)?, -DAY_1, &[true, true, false]);
        check_all(julian_0(365)?, &[-1, 0, 1], julian_0(0)?, 0, &[true, true, false]);

        check_all(julian_1(90)?, &[-1, 0, 1], julian_0(90)?, 0, &[true, true, false]);
        check_all(julian_1(365)?, &[-1, 0, 1], julian_0(0)?, 0, &[true, true, true]);

        check_all(julian_0(89)?, &[-1, 0, 1], julian_1(90)?, 0, &[true, true, false]);
        check_all(julian_0(364)?, &[-1, 0, 1], julian_1(1)?, -DAY_1, &[true, true, false]);
        check_all(julian_0(365)?, &[-1, 0, 1], julian_1(1)?, 0, &[true, true, false]);

        check_all(mwd(1, 4, 0)?, &[-1, 0, 1], julian_1(28)?, 0, &[true, true, false]);
        check_all(mwd(2, 5, 0)?, &[-1, 0, 1], julian_1(60)?, -DAY_1, &[true, true, false]);
        check_all(mwd(12, 5, 0)?, &[-1, 0, 1], julian_1(1)?, -DAY_1, &[true, true, false]);
        check_all(mwd(12, 5, 0)?, &[DAY_3 - 1, DAY_3, DAY_3 + 1], julian_1(1)?, -DAY_4, &[false, true, true]);

        check_all(mwd(1, 4, 0)?, &[-1, 0, 1], julian_0(27)?, 0, &[true, true, false]);
        check_all(mwd(2, 5, 0)?, &[-1, 0, 1], julian_0(58)?, DAY_1, &[true, true, false]);
        check_all(mwd(2, 4, 0)?, &[-1, 0, 1], julian_0(59)?, -DAY_1, &[true, true, false]);
        check_all(mwd(2, 5, 0)?, &[-1, 0, 1], julian_0(59)?, 0, &[true, true, false]);
        check_all(mwd(12, 5, 0)?, &[-1, 0, 1], julian_0(0)?, -DAY_1, &[true, true, false]);
        check_all(mwd(12, 5, 0)?, &[DAY_3 - 1, DAY_3, DAY_3 + 1], julian_0(0)?, -DAY_4, &[false, true, true]);

        check_all(julian_1(1)?, &[-1, 0, 1], mwd(1, 1, 0)?, 0, &[true, true, false]);
        check_all(julian_1(53)?, &[-1, 0, 1], mwd(2, 5, 0)?, 0, &[true, true, false]);
        check_all(julian_1(365)?, &[-1, 0, 1], mwd(1, 1, 0)?, -DAY_1, &[true, true, false]);
        check_all(julian_1(365)?, &[DAY_3 - 1, DAY_3, DAY_3 + 1], mwd(1, 1, 0)?, -DAY_4, &[false, true, true]);

        check_all(julian_0(0)?, &[-1, 0, 1], mwd(1, 1, 0)?, 0, &[true, true, false]);
        check_all(julian_0(52)?, &[-1, 0, 1], mwd(2, 5, 0)?, 0, &[true, true, false]);
        check_all(julian_0(59)?, &[-1, 0, 1], mwd(3, 1, 0)?, 0, &[true, true, false]);
        check_all(julian_0(59)?, &[-DAY_3 - 1, -DAY_3, -DAY_3 + 1], mwd(2, 5, 0)?, DAY_4, &[true, true, false]);
        check_all(julian_0(364)?, &[-1, 0, 1], mwd(1, 1, 0)?, -DAY_1, &[true, true, false]);
        check_all(julian_0(365)?, &[-1, 0, 1], mwd(1, 1, 0)?, 0, &[true, true, false]);
        check_all(julian_0(364)?, &[DAY_4 - 1, DAY_4, DAY_4 + 1], mwd(1, 1, 0)?, -DAY_4, &[false, true, true]);
        check_all(julian_0(365)?, &[DAY_3 - 1, DAY_3, DAY_3 + 1], mwd(1, 1, 0)?, -DAY_4, &[false, true, true]);

        let months_per_year = MONTHS_PER_YEAR as u8;
        for i in 0..months_per_year - 1 {
            let month = i + 1;
            let month_1 = (i + 1) % months_per_year + 1;
            let month_2 = (i + 2) % months_per_year + 1;

            assert!(check(mwd(month, 1, 0)?, 0, mwd(month_2, 1, 0)?, 0));
            assert!(check(mwd(month, 3, 0)?, DAY_4, mwd(month, 4, 0)?, -DAY_3));

            check_all(mwd(month, 5, 0)?, &[-1, 0, 1], mwd(month, 5, 0)?, 0, &[true, true, true]);
            check_all(mwd(month, 4, 0)?, &[-1, 0, 1], mwd(month, 5, 0)?, 0, &[true, true, false]);
            check_all(mwd(month, 4, 0)?, &[DAY_4 - 1, DAY_4, DAY_4 + 1], mwd(month_1, 1, 0)?, -DAY_3, &[true, true, false]);
            check_all(mwd(month, 5, 0)?, &[DAY_4 - 1, DAY_4, DAY_4 + 1], mwd(month_1, 1, 0)?, -DAY_3, &[true, true, true]);
            check_all(mwd(month, 5, 0)?, &[-1, 0, 1], mwd(month_1, 5, 0)?, 0, &[true, true, true]);
            check_all(mwd(month, 3, 2)?, &[-1, 0, 1], mwd(month, 4, 3)?, -DAY_1, &[true, true, false]);
            check_all(mwd(month, 5, 2)?, &[-1, 0, 1], mwd(month, 5, 3)?, -DAY_1, &[false, true, true]);
            check_all(mwd(month, 5, 2)?, &[-1, 0, 1], mwd(month_1, 1, 3)?, -DAY_1, &[true, true, false]);
            check_all(mwd(month, 5, 2)?, &[-1, 0, 1], mwd(month_1, 5, 3)?, 0, &[true, true, true]);
        }

        check_all(mwd(2, 4, 2)?, &[-1, 0, 1], mwd(2, 5, 3)?, -DAY_1, &[false, true, true]);

        check_all(mwd(3, 4, 2)?, &[-1, 0, 1], mwd(3, 5, 4)?, -DAY_2, &[true, true, false]);
        check_all(mwd(3, 4, 2)?, &[-1, 0, 1], mwd(3, 5, 5)?, -DAY_3, &[true, true, true]);
        check_all(mwd(3, 4, 2)?, &[-1, 0, 1], mwd(3, 5, 6)?, -DAY_4, &[false, true, true]);

        check_all(mwd(4, 4, 2)?, &[-1, 0, 1], mwd(4, 5, 3)?, -DAY_1, &[true, true, false]);
        check_all(mwd(4, 4, 2)?, &[-1, 0, 1], mwd(4, 5, 4)?, -DAY_2, &[true, true, true]);
        check_all(mwd(4, 4, 2)?, &[-1, 0, 1], mwd(4, 5, 5)?, -DAY_3, &[false, true, true]);

        check_all(mwd(2, 4, 2)?, &[DAY_5 - 1, DAY_5, DAY_5 + 1], mwd(3, 1, 3)?, -DAY_3, &[false, true, true]);

        check_all(mwd(3, 4, 2)?, &[DAY_5 - 1, DAY_5, DAY_5 + 1], mwd(4, 1, 4)?, -DAY_4, &[true, true, false]);
        check_all(mwd(3, 4, 2)?, &[DAY_5 - 1, DAY_5, DAY_5 + 1], mwd(4, 1, 5)?, -DAY_5, &[true, true, true]);
        check_all(mwd(3, 4, 2)?, &[DAY_5 - 1, DAY_5, DAY_5 + 1], mwd(4, 1, 6)?, -DAY_6, &[false, true, true]);

        check_all(mwd(4, 4, 2)?, &[DAY_5 - 1, DAY_5, DAY_5 + 1], mwd(5, 1, 3)?, -DAY_3, &[true, true, false]);
        check_all(mwd(4, 4, 2)?, &[DAY_5 - 1, DAY_5, DAY_5 + 1], mwd(5, 1, 4)?, -DAY_4, &[true, true, true]);
        check_all(mwd(4, 4, 2)?, &[DAY_5 - 1, DAY_5, DAY_5 + 1], mwd(5, 1, 5)?, -DAY_5, &[false, true, true]);

        Ok(())
    }

    #[test]
    fn test_rule_day() -> Result<()> {
        let rule_day_j1 = RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(60)?);
        assert_eq!(rule_day_j1.transition_date(2000), (3, 1));
        assert_eq!(rule_day_j1.transition_date(2001), (3, 1));
        assert_eq!(rule_day_j1.unix_time(2000, 43200), 951912000);

        let rule_day_j0 = RuleDay::Julian0WithLeap(Julian0WithLeap::new(59)?);
        assert_eq!(rule_day_j0.transition_date(2000), (2, 29));
        assert_eq!(rule_day_j0.transition_date(2001), (3, 1));
        assert_eq!(rule_day_j0.unix_time(2000, 43200), 951825600);

        let rule_day_j0_max = RuleDay::Julian0WithLeap(Julian0WithLeap::new(365)?);
        assert_eq!(rule_day_j0_max.transition_date(2000), (12, 31));
        assert_eq!(rule_day_j0_max.transition_date(2001), (12, 32));

        assert_eq!(
            RuleDay::Julian0WithLeap(Julian0WithLeap::new(365)?).unix_time(2000, 0),
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(365)?).unix_time(2000, 0)
        );

        assert_eq!(
            RuleDay::Julian0WithLeap(Julian0WithLeap::new(365)?).unix_time(1999, 0),
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(1)?).unix_time(2000, 0),
        );

        let rule_day_mwd = RuleDay::MonthWeekDay(MonthWeekDay::new(2, 5, 2)?);
        assert_eq!(rule_day_mwd.transition_date(2000), (2, 29));
        assert_eq!(rule_day_mwd.transition_date(2001), (2, 27));
        assert_eq!(rule_day_mwd.unix_time(2000, 43200), 951825600);
        assert_eq!(rule_day_mwd.unix_time(2001, 43200), 983275200);

        Ok(())
    }

    #[test]
    fn test_transition_rule() -> Result<()> {
        let transition_rule_fixed = TransitionRule::Fixed(LocalTimeType::new(-36000, false, None)?);
        assert_eq!(transition_rule_fixed.find_local_time_type(0)?.ut_offset(), -36000);

        let transition_rule_dst = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(43200, false, Some(b"NZST"))?,
            LocalTimeType::new(46800, true, Some(b"NZDT"))?,
            RuleDay::MonthWeekDay(MonthWeekDay::new(10, 1, 0)?),
            7200,
            RuleDay::MonthWeekDay(MonthWeekDay::new(3, 3, 0)?),
            7200,
        )?);

        assert_eq!(transition_rule_dst.find_local_time_type(953384399)?.ut_offset(), 46800);
        assert_eq!(transition_rule_dst.find_local_time_type(953384400)?.ut_offset(), 43200);
        assert_eq!(transition_rule_dst.find_local_time_type(970322399)?.ut_offset(), 43200);
        assert_eq!(transition_rule_dst.find_local_time_type(970322400)?.ut_offset(), 46800);

        let transition_rule_negative_dst = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(3600, false, Some(b"IST"))?,
            LocalTimeType::new(0, true, Some(b"GMT"))?,
            RuleDay::MonthWeekDay(MonthWeekDay::new(10, 5, 0)?),
            7200,
            RuleDay::MonthWeekDay(MonthWeekDay::new(3, 5, 0)?),
            3600,
        )?);

        assert_eq!(transition_rule_negative_dst.find_local_time_type(954032399)?.ut_offset(), 0);
        assert_eq!(transition_rule_negative_dst.find_local_time_type(954032400)?.ut_offset(), 3600);
        assert_eq!(transition_rule_negative_dst.find_local_time_type(972781199)?.ut_offset(), 3600);
        assert_eq!(transition_rule_negative_dst.find_local_time_type(972781200)?.ut_offset(), 0);

        let transition_rule_negative_time_1 = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(0, false, None)?,
            LocalTimeType::new(0, true, None)?,
            RuleDay::Julian0WithLeap(Julian0WithLeap::new(100)?),
            0,
            RuleDay::Julian0WithLeap(Julian0WithLeap::new(101)?),
            -86500,
        )?);

        assert!(transition_rule_negative_time_1.find_local_time_type(8639899)?.is_dst());
        assert!(!transition_rule_negative_time_1.find_local_time_type(8639900)?.is_dst());
        assert!(!transition_rule_negative_time_1.find_local_time_type(8639999)?.is_dst());
        assert!(transition_rule_negative_time_1.find_local_time_type(8640000)?.is_dst());

        let transition_rule_negative_time_2 = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(-10800, false, Some(b"-03"))?,
            LocalTimeType::new(-7200, true, Some(b"-02"))?,
            RuleDay::MonthWeekDay(MonthWeekDay::new(3, 5, 0)?),
            -7200,
            RuleDay::MonthWeekDay(MonthWeekDay::new(10, 5, 0)?),
            -3600,
        )?);

        assert_eq!(transition_rule_negative_time_2.find_local_time_type(954032399)?.ut_offset(), -10800);
        assert_eq!(transition_rule_negative_time_2.find_local_time_type(954032400)?.ut_offset(), -7200);
        assert_eq!(transition_rule_negative_time_2.find_local_time_type(972781199)?.ut_offset(), -7200);
        assert_eq!(transition_rule_negative_time_2.find_local_time_type(972781200)?.ut_offset(), -10800);

        let transition_rule_all_year_dst = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(-18000, false, Some(b"EST"))?,
            LocalTimeType::new(-14400, true, Some(b"EDT"))?,
            RuleDay::Julian0WithLeap(Julian0WithLeap::new(0)?),
            0,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(365)?),
            90000,
        )?);

        assert_eq!(transition_rule_all_year_dst.find_local_time_type(946702799)?.ut_offset(), -14400);
        assert_eq!(transition_rule_all_year_dst.find_local_time_type(946702800)?.ut_offset(), -14400);

        Ok(())
    }

    #[test]
    fn test_transition_rule_overflow() -> Result<()> {
        let transition_rule_1 = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(-1, false, None)?,
            LocalTimeType::new(-1, true, None)?,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(365)?),
            0,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(1)?),
            0,
        )?);

        let transition_rule_2 = TransitionRule::Alternate(AlternateTime::new(
            LocalTimeType::new(1, false, None)?,
            LocalTimeType::new(1, true, None)?,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(365)?),
            0,
            RuleDay::Julian1WithoutLeap(Julian1WithoutLeap::new(1)?),
            0,
        )?);

        assert!(matches!(transition_rule_1.find_local_time_type(i64::MIN), Err(OutOfRangeError(_))));
        assert!(matches!(transition_rule_2.find_local_time_type(i64::MAX), Err(OutOfRangeError(_))));

        Ok(())
    }
}
