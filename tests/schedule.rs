//! Time zones and calendar schedules (docs/spec/10-daemon.md § scheduler).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use cestmoi::daemon::calendar::Calendar;
use cestmoi::daemon::zone::Zone;
use cestmoi::host::{FixtureHost, format_rfc3339, parse_rfc3339, unix_seconds};

fn at(time: &str) -> i64 {
    unix_seconds(parse_rfc3339(time).unwrap_or_else(|| panic!("bad time {time}")))
}

fn show(secs: i64) -> String {
    format_rfc3339(std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(secs).unwrap()))
}

/// America/New_York, as the cestmoi fixture has it.
fn new_york() -> Zone {
    Zone::load(&FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap())
}

/// A TZif file with only the rule for the future.
fn zone_of(rule: &str) -> Zone {
    let header = |version: u8| [b"TZif".as_slice(), &[version], &[0u8; 15], &[0u8; 24]].concat();
    Zone::parse(&[header(b'2'), header(b'2'), format!("\n{rule}\n").into_bytes()].concat()).unwrap()
}

#[test]
fn the_servers_zone_gives_the_offset_of_any_moment() {
    let zone = new_york();
    assert_eq!(zone.offset_at(at("2026-11-26T17:00:00Z")), -5 * 3600);
    assert_eq!(zone.offset_at(at("2026-07-01T12:00:00Z")), -4 * 3600);
    // clocks go forward on the second Sunday of March at 02:00 EST, back on the first of November at 02:00 EDT
    assert_eq!(zone.offset_at(at("2026-03-08T06:59:59Z")), -5 * 3600);
    assert_eq!(zone.offset_at(at("2026-03-08T07:00:00Z")), -4 * 3600);
    assert_eq!(zone.offset_at(at("2026-11-01T05:59:59Z")), -4 * 3600);
    assert_eq!(zone.offset_at(at("2026-11-01T06:00:00Z")), -5 * 3600);
    // before the file's first transition, and after its last one (the rule at its end takes over)
    assert_eq!(zone.offset_at(at("2000-07-01T12:00:00Z")), -5 * 3600);
    assert_eq!(zone.offset_at(at("2045-07-01T12:00:00Z")), -4 * 3600);
    assert_eq!(zone.offset_at(at("2045-01-01T12:00:00Z")), -5 * 3600);
    assert_eq!(zone.offset_at(at("2045-03-12T07:00:00Z")), -4 * 3600, "second Sunday of March 2045");
    assert_eq!(zone.offset_at(at("2045-03-12T06:59:59Z")), -5 * 3600);
}

#[test]
fn local_times_that_happen_twice_or_never_have_one_moment() {
    let zone = new_york();
    let local = |time: &str| at(time);
    // an ordinary time
    assert_eq!(show(zone.to_utc(local("2026-11-26T12:00:00Z"))), "2026-11-26T17:00:00Z");
    assert_eq!(show(zone.to_local(at("2026-11-26T17:00:00Z"))), "2026-11-26T12:00:00Z");
    // 01:30 on the night clocks go back happens twice: the first one counts
    assert_eq!(show(zone.to_utc(local("2026-11-01T01:30:00Z"))), "2026-11-01T05:30:00Z");
    // 02:30 on the night clocks go forward never happens: an hour after 01:30
    assert_eq!(show(zone.to_utc(local("2026-03-08T02:30:00Z"))), "2026-03-08T07:30:00Z");
}

#[test]
fn rules_for_the_future_cover_the_forms_zone_files_use() {
    // southern hemisphere: daylight saving over the new year
    let sydney = zone_of("AEST-10AEDT,M10.1.0,M4.1.0/3");
    assert_eq!(sydney.offset_at(at("2026-01-15T00:00:00Z")), 11 * 3600);
    assert_eq!(sydney.offset_at(at("2026-07-15T00:00:00Z")), 10 * 3600);
    assert_eq!(sydney.offset_at(at("2026-10-03T15:59:59Z")), 10 * 3600);
    assert_eq!(sydney.offset_at(at("2026-10-03T16:00:00Z")), 11 * 3600, "first Sunday of October, 02:00 standard time");
    // no daylight saving; quoted names; minutes
    assert_eq!(zone_of("<+03>-3").offset_at(at("2026-06-01T00:00:00Z")), 3 * 3600);
    assert_eq!(zone_of("UTC0").offset_at(at("2026-06-01T00:00:00Z")), 0);
    assert_eq!(zone_of("IST-5:30").offset_at(at("2026-06-01T00:00:00Z")), 5 * 3600 + 1800);
    // last week of a month (M3.5.0), an explicit daylight offset and times
    let berlin = zone_of("CET-1CEST-2,M3.5.0/2,M10.5.0/3");
    assert_eq!(berlin.offset_at(at("2026-03-29T00:59:59Z")), 3600);
    assert_eq!(berlin.offset_at(at("2026-03-29T01:00:00Z")), 7200, "last Sunday of March");
    assert_eq!(berlin.offset_at(at("2026-10-25T01:00:00Z")), 3600);
    // days of the year
    let julian = zone_of("AAA3BBB,J60/0,J300/0");
    assert_eq!(julian.offset_at(at("2028-03-01T03:00:00Z")), -2 * 3600, "J60 is March 1 also in a leap year");
    assert_eq!(julian.offset_at(at("2028-02-29T12:00:00Z")), -3 * 3600);
}

#[test]
fn what_is_not_a_zone_file_is_utc() {
    assert!(Zone::parse(b"not a zone file").is_none());
    assert!(Zone::parse(b"").is_none());
    // the minimal fixture has no zone data at all
    let zone = Zone::load(&common::minimal());
    assert_eq!(zone, Zone::utc());
    assert_eq!(zone.offset_at(at("2026-07-01T00:00:00Z")), 0);
    // version 1 files (32-bit times, no rule): the last offset stays
    let mut v1 =
        [b"TZif".as_slice(), &[0u8; 16], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 4]]
            .concat();
    v1.extend(1_000_000_000_i32.to_be_bytes());
    v1.push(1);
    v1.extend(
        [3600_i32.to_be_bytes().as_slice(), &[0, 0], 7200_i32.to_be_bytes().as_slice(), &[1, 0], b"ABC\0"].concat(),
    );
    let zone = Zone::parse(&v1).unwrap();
    assert_eq!(
        (zone.offset_at(999_999_999), zone.offset_at(1_000_000_000), zone.offset_at(at("2040-01-01T00:00:00Z"))),
        (3600, 7200, 7200)
    );
}

/// The next time a schedule is due after noon on Thursday 26 November 2026 in New York.
fn next(schedule: &str) -> String {
    let calendar = Calendar::parse(schedule).unwrap_or_else(|| panic!("{schedule} does not parse"));
    calendar.next_after(at("2026-11-26T17:00:00Z"), &new_york()).map_or_else(|| "never".to_string(), show)
}

#[test]
fn schedules_are_due_at_local_time() {
    // the forms the config uses
    assert_eq!(next("03:30"), "2026-11-27T08:30:00Z");
    assert_eq!(next("*-*-* 03:30:00"), "2026-11-27T08:30:00Z");
    assert_eq!(next("Sun 04:00"), "2026-11-29T09:00:00Z");
    assert_eq!(next("12:00"), "2026-11-27T17:00:00Z", "strictly after: today's noon is now");
    assert_eq!(next("12:00:01"), "2026-11-26T17:00:01Z");
    // shorthands
    assert_eq!(next("minutely"), "2026-11-26T17:01:00Z");
    assert_eq!(next("hourly"), "2026-11-26T18:00:00Z");
    assert_eq!(next("daily"), "2026-11-27T05:00:00Z");
    assert_eq!(next("weekly"), "2026-11-30T05:00:00Z");
    assert_eq!(next("monthly"), "2026-12-01T05:00:00Z");
    assert_eq!(next("quarterly"), "2027-01-01T05:00:00Z");
    assert_eq!(next("yearly"), "2027-01-01T05:00:00Z");
    assert_eq!(next("annually"), "2027-01-01T05:00:00Z");
    // weekdays: lists, ranges, ranges around the weekend, long names
    assert_eq!(next("Mon..Fri 08:00"), "2026-11-27T13:00:00Z");
    assert_eq!(next("Sat,Sun 10:00"), "2026-11-28T15:00:00Z");
    assert_eq!(next("Sat..Mon 10:00"), "2026-11-28T15:00:00Z");
    assert_eq!(next("Thursday 13:00"), "2026-11-26T18:00:00Z");
    assert_eq!(next("Thu 11:00"), "2026-12-03T16:00:00Z");
    // dates, lists, ranges and steps
    assert_eq!(next("12-25 09:00"), "2026-12-25T14:00:00Z");
    assert_eq!(next("2027-07-04 09:00"), "2027-07-04T13:00:00Z");
    assert_eq!(next("*-*-01,15 00:00"), "2026-12-01T05:00:00Z");
    assert_eq!(next("*:0/15"), "2026-11-26T17:15:00Z");
    assert_eq!(next("*:*:0/20"), "2026-11-26T17:00:20Z");
    assert_eq!(next("8..10:30"), "2026-11-27T13:30:00Z");
    assert_eq!(next("0/6:00"), "2026-11-26T23:00:00Z", "00, 06, 12, 18 local");
    assert_eq!(next("Fri *-*-13 12:00"), "2027-08-13T16:00:00Z", "the next Friday the 13th");
    assert_eq!(next("02-30 12:00"), "never");
}

#[test]
fn schedules_follow_daylight_saving() {
    let zone = new_york();
    let nightly = Calendar::parse("03:30").unwrap();
    // 03:30 is 08:30 UTC in winter and 07:30 UTC in summer
    assert_eq!(show(nightly.next_after(at("2026-03-07T09:00:00Z"), &zone).unwrap()), "2026-03-08T07:30:00Z");
    assert_eq!(show(nightly.next_after(at("2026-10-31T09:00:00Z"), &zone).unwrap()), "2026-11-01T08:30:00Z");
    // a time in the hour that does not exist still runs that night, once
    let early = Calendar::parse("02:30").unwrap();
    let first = early.next_after(at("2026-03-07T09:00:00Z"), &zone).unwrap();
    assert_eq!(show(first), "2026-03-08T07:30:00Z");
    assert_eq!(show(early.next_after(first, &zone).unwrap()), "2026-03-09T06:30:00Z");
    // a time in the hour that happens twice runs once
    let late = Calendar::parse("01:30").unwrap();
    let first = late.next_after(at("2026-10-31T09:00:00Z"), &zone).unwrap();
    assert_eq!(show(first), "2026-11-01T05:30:00Z");
    assert_eq!(show(late.next_after(first, &zone).unwrap()), "2026-11-02T06:30:00Z");
}

#[test]
fn what_is_not_a_schedule_does_not_parse() {
    for bad in
        ["", "nonsense", "Mon", "03", "Mon Tue 03:30", "Xyz 03:30", "03:30:00:00x", "a:b", "*/0:00", "1-2-3-4 03:30"]
    {
        assert!(Calendar::parse(bad).is_none(), "{bad:?} should not parse");
    }
}
