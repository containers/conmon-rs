use tz::*;

fn main() -> Result<()> {
    #[cfg(feature = "std")]
    {
        //
        // TimeZone
        //

        // 2000-01-01T00:00:00Z
        let unix_time = 946684800;

        // Get UTC time zone
        let time_zone_utc = TimeZone::utc();
        println!("{:?}", time_zone_utc.find_local_time_type(unix_time)?);

        // Get fixed time zone at GMT-1
        let time_zone_fixed = TimeZone::fixed(-3600)?;
        println!("{:?}", time_zone_fixed.find_local_time_type(unix_time)?.ut_offset());

        // Get local time zone (UNIX only)
        let time_zone_local = TimeZone::local()?;
        println!("{:?}", time_zone_local.find_local_time_type(unix_time)?.ut_offset());

        // Get the current local time type
        let current_local_time_type = time_zone_local.find_current_local_time_type()?;
        println!("{:?}", current_local_time_type);

        // Get time zone from a TZ string:
        // From an absolute file
        let _ = TimeZone::from_posix_tz("/usr/share/zoneinfo/Pacific/Auckland");
        // From a file relative to the system timezone directory
        let _ = TimeZone::from_posix_tz("Pacific/Auckland");
        // From a time zone description
        TimeZone::from_posix_tz("HST10")?;
        TimeZone::from_posix_tz("<-03>3")?;
        TimeZone::from_posix_tz("NZST-12:00:00NZDT-13:00:00,M10.1.0,M3.3.0")?;
        // Use a leading colon to force searching for a corresponding file
        let _ = TimeZone::from_posix_tz(":UTC");

        //
        // DateTime
        //

        // Get the current UTC date time
        let current_utc_date_time = UtcDateTime::now()?;
        println!("{:?}", current_utc_date_time);

        // Create a new UTC date time (2000-01-01T00:00:00.123456789Z)
        let utc_date_time = UtcDateTime::new(2000, 1, 1, 0, 0, 0, 123_456_789)?;
        println!("{}", utc_date_time);
        println!("{:?}", utc_date_time);

        // Create a new UTC date time from a Unix time with nanoseconds (2000-01-01T00:00:00.123456789Z)
        let other_utc_date_time = UtcDateTime::from_timespec(946684800, 123_456_789)?;
        println!("{}", other_utc_date_time);
        println!("{:?}", other_utc_date_time);

        // Project the UTC date time to a time zone
        let date_time = utc_date_time.project(TimeZone::fixed(-3600)?.as_ref())?;
        println!("{}", date_time);
        println!("{:#?}", date_time);

        // Project the date time to another time zone
        let other_date_time = date_time.project(TimeZone::fixed(3600)?.as_ref())?;
        println!("{}", other_date_time);
        println!("{:#?}", other_date_time);

        // Create a new date time from a Unix time with nanoseconds and a time zone (2000-01-01T00:00:00.123456789Z)
        let another_date_time = DateTime::from_timespec(946684800, 123_456_789, TimeZone::fixed(86400)?.as_ref())?;
        println!("{}", another_date_time);
        println!("{:#?}", another_date_time);

        // Get the corresponding UTC Unix times with nanoseconds
        println!("{:?}", (utc_date_time.unix_time(), utc_date_time.nanoseconds()));
        println!("{:?}", (other_utc_date_time.unix_time(), other_utc_date_time.nanoseconds()));
        println!("{:?}", (date_time.unix_time(), date_time.nanoseconds()));
        println!("{:?}", (other_date_time.unix_time(), other_date_time.nanoseconds()));

        // Nanoseconds are always added towards the future
        let neg_utc_date_time = UtcDateTime::from_timespec(-1, 123_456_789)?;
        println!("{}", neg_utc_date_time);
        println!("{}", neg_utc_date_time.total_nanoseconds());

        // Get the current date time at the local time zone (UNIX only)
        let time_zone_local = TimeZone::local()?;
        let date_time = DateTime::now(time_zone_local.as_ref())?;
        println!("{:#?}", date_time);

        // Create a new date time with an UTC offset (2000-01-01T01:00:00.123456789+01:00)
        let date_time = DateTime::new(2000, 1, 1, 1, 0, 0, 123_456_789, LocalTimeType::with_ut_offset(3600)?)?;
        println!("{:#?}", date_time);

        //
        // Find the possible date times corresponding to a date, a time and a time zone
        //
        let time_zone = TimeZone::from_posix_tz("CET-1CEST,M3.5.0,M10.5.0/3")?;

        // Found date time is unique
        let found_date_times = DateTime::find(2000, 1, 1, 0, 0, 0, 0, time_zone.as_ref())?;
        println!("{:#?}", found_date_times);
        println!("{:#?}", found_date_times.unique());
        println!("{:#?}", found_date_times.earliest());
        println!("{:#?}", found_date_times.latest());

        // Found date time was skipped by a forward transition
        let found_date_times = DateTime::find(2000, 3, 26, 2, 30, 0, 0, time_zone.as_ref())?;
        println!("{:#?}", found_date_times);
        println!("{:#?}", found_date_times.unique());
        println!("{:#?}", found_date_times.earliest());
        println!("{:#?}", found_date_times.latest());

        // Found date time is ambiguous because of a backward transition
        let found_date_times = DateTime::find(2000, 10, 29, 2, 30, 0, 0, time_zone.as_ref())?;
        println!("{:#?}", found_date_times);
        println!("{:#?}", found_date_times.unique());
        println!("{:#?}", found_date_times.earliest());
        println!("{:#?}", found_date_times.latest());
    }

    Ok(())
}
