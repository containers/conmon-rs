fn main() -> tz::Result<()> {
    #[cfg(feature = "const")]
    {
        use tz::datetime::*;
        use tz::timezone::*;

        macro_rules! unwrap {
            ($x:expr) => {
                match $x {
                    Ok(x) => x,
                    Err(_) => [][0],
                }
            };
        }

        macro_rules! to_const {
            ($type:ty, $x:expr) => {{
                const TMP: $type = $x;
                TMP
            }};
        }

        const TIME_ZONE_REF: TimeZoneRef = unwrap!(TimeZoneRef::new(
            &[
                Transition::new(-2334101314, 1),
                Transition::new(-1157283000, 2),
                Transition::new(-1155436200, 1),
                Transition::new(-880198200, 3),
                Transition::new(-769395600, 4),
                Transition::new(-765376200, 1),
                Transition::new(-712150200, 5),
            ],
            to_const!(
                &[LocalTimeType],
                &[
                    unwrap!(LocalTimeType::new(-37886, false, Some(b"LMT"))),
                    unwrap!(LocalTimeType::new(-37800, false, Some(b"HST"))),
                    unwrap!(LocalTimeType::new(-34200, true, Some(b"HDT"))),
                    unwrap!(LocalTimeType::new(-34200, true, Some(b"HWT"))),
                    unwrap!(LocalTimeType::new(-34200, true, Some(b"HPT"))),
                    unwrap!(LocalTimeType::new(-36000, false, Some(b"HST"))),
                ]
            ),
            &[
                LeapSecond::new(78796800, 1),
                LeapSecond::new(94694401, 2),
                LeapSecond::new(126230402, 3),
                LeapSecond::new(157766403, 4),
                LeapSecond::new(189302404, 5),
                LeapSecond::new(220924805, 6),
            ],
            to_const!(
                &Option<TransitionRule>,
                &Some(TransitionRule::Alternate(unwrap!(AlternateTime::new(
                    unwrap!(LocalTimeType::new(-36000, false, Some(b"HST"))),
                    unwrap!(LocalTimeType::new(-34200, true, Some(b"HPT"))),
                    RuleDay::MonthWeekDay(unwrap!(MonthWeekDay::new(10, 5, 0))),
                    93600,
                    RuleDay::MonthWeekDay(unwrap!(MonthWeekDay::new(3, 4, 4))),
                    7200,
                ))))
            ),
        ));

        const UTC: TimeZoneRef = TimeZoneRef::utc();

        const UNIX_EPOCH: UtcDateTime = unwrap!(UtcDateTime::from_timespec(0, 0));
        const UTC_DATE_TIME: UtcDateTime = unwrap!(UtcDateTime::new(2000, 1, 1, 0, 0, 0, 1000));

        const DATE_TIME: DateTime = unwrap!(DateTime::new(2000, 1, 1, 1, 0, 0, 1000, unwrap!(LocalTimeType::with_ut_offset(3600))));

        const DATE_TIME_1: DateTime = unwrap!(UTC_DATE_TIME.project(TIME_ZONE_REF));
        const DATE_TIME_2: DateTime = unwrap!(DATE_TIME_1.project(UTC));

        println!("{:#?}", TIME_ZONE_REF);
        println!("{:?}", TIME_ZONE_REF.find_local_time_type(0));

        println!("{:?}", UNIX_EPOCH);
        println!("{:?}", UTC_DATE_TIME);

        println!("{:#?}", DATE_TIME);

        println!("{:#?}", DATE_TIME_1);
        println!("{:#?}", DATE_TIME_2);
    }

    Ok(())
}
