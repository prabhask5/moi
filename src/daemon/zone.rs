//! The server's time zone, read from its zoneinfo file (TZif, RFC 8536): schedules such as "03:30" mean
//! local time, daylight saving included. No time-zone database is compiled in.

use std::path::Path;

use crate::host::{self, Host, civil_from_days, days_from_civil};

/// When daylight saving starts or ends in a year: `Mm.w.d`, `Jn` or `n`, at a local time of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// Month, week (5 = last), weekday (0 = Sunday).
    MonthWeekDay(i64, i64, i64),
    /// Day of the year 1..=365, February 29 never counted.
    Julian(i64),
    /// Day of the year 0..=365, February 29 counted.
    DayOfYear(i64),
}

/// Daylight saving time: its offset, when it starts and when it ends (each with its local time of day).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Daylight {
    offset: i64,
    start: (Change, i64),
    end: (Change, i64),
}

/// The rule for times after the last recorded transition (the TZ string at the end of the file).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    standard: i64,
    daylight: Option<Daylight>,
}

/// Offsets from UTC over time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    /// (first second it applies, seconds east of UTC), ascending.
    transitions: Vec<(i64, i64)>,
    /// Before the first transition.
    initial: i64,
    rule: Option<Rule>,
}

fn be(bytes: &[u8]) -> i64 {
    bytes.iter().fold(0_i64, |acc, b| (acc << 8) | i64::from(*b))
}

/// `[+-]hh[:mm[:ss]]` at the start of `text`; returns seconds and the rest.
fn hms(text: &str) -> Option<(i64, &str)> {
    let (sign, rest) = match text.as_bytes().first()? {
        b'-' => (-1, &text[1..]),
        b'+' => (1, &text[1..]),
        _ => (1, text),
    };
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != ':').unwrap_or(rest.len());
    let mut parts = rest[..end].split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    let seconds: i64 = parts.next().map_or(Some(0), |s| s.parse().ok())?;
    Some((sign * (hours * 3600 + minutes * 60 + seconds), &rest[end..]))
}

/// A zone abbreviation (`EST`, `<+03>`); returns the rest.
fn abbreviation(text: &str) -> Option<&str> {
    if let Some(quoted) = text.strip_prefix('<') {
        return quoted.split_once('>').map(|(_, rest)| rest);
    }
    let end = text.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(text.len());
    (end >= 3).then(|| &text[end..])
}

fn change(text: &str) -> Option<(Change, i64)> {
    let (date, time) = text.split_once('/').unwrap_or((text, "2"));
    let time = hms(time)?.0;
    let when = if let Some(mwd) = date.strip_prefix('M') {
        let mut parts = mwd.split('.').map(str::parse::<i64>);
        Change::MonthWeekDay(parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?)
    } else if let Some(julian) = date.strip_prefix('J') {
        Change::Julian(julian.parse().ok()?)
    } else {
        Change::DayOfYear(date.parse().ok()?)
    };
    Some((when, time))
}

impl Rule {
    /// A POSIX `TZ` string: `EST5EDT,M3.2.0,M11.1.0`. The offset in it counts west of UTC.
    fn parse(text: &str) -> Option<Self> {
        let rest = abbreviation(text)?;
        let (west, rest) = hms(rest)?;
        let standard = -west;
        if rest.is_empty() {
            return Some(Self { standard, daylight: None });
        }
        let rest = abbreviation(rest)?;
        let (daylight, rest) = match rest.as_bytes().first() {
            Some(b',') | None => (standard + 3600, rest),
            Some(_) => {
                let (west, rest) = hms(rest)?;
                (-west, rest)
            }
        };
        let mut rules = rest.strip_prefix(',')?.split(',');
        Some(Self {
            standard,
            daylight: Some(Daylight { offset: daylight, start: change(rules.next()?)?, end: change(rules.next()?)? }),
        })
    }

    /// Days since the epoch of a change in a year.
    fn day(when: Change, year: i64) -> i64 {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        match when {
            Change::Julian(n) => days_from_civil(year, 1, 1) + n - 1 + i64::from(leap && n > 59),
            Change::DayOfYear(n) => days_from_civil(year, 1, 1) + n,
            Change::MonthWeekDay(month, week, weekday) => {
                let first = days_from_civil(year, month, 1);
                // 1970-01-01 was a Thursday (4)
                let first_weekday = (first + 4).rem_euclid(7);
                let mut day = first + (weekday - first_weekday).rem_euclid(7) + (week - 1) * 7;
                let next_month =
                    if month == 12 { days_from_civil(year + 1, 1, 1) } else { days_from_civil(year, month + 1, 1) };
                while day >= next_month {
                    day -= 7;
                }
                day
            }
        }
    }

    fn offset_at(&self, utc: i64) -> i64 {
        let Some(Daylight { offset: daylight, start: (start, start_time), end: (end, end_time) }) = self.daylight
        else {
            return self.standard;
        };
        let year = civil_from_days(utc.div_euclid(86_400)).0;
        // the start is given in standard time, the end in daylight time
        let starts = Self::day(start, year) * 86_400 + start_time - self.standard;
        let ends = Self::day(end, year) * 86_400 + end_time - daylight;
        let in_daylight = if starts < ends { (starts..ends).contains(&utc) } else { !(ends..starts).contains(&utc) };
        if in_daylight { daylight } else { self.standard }
    }
}

impl Zone {
    pub fn utc() -> Self {
        Self { transitions: Vec::new(), initial: 0, rule: None }
    }

    /// Parse a TZif file. `None` if it is not one.
    pub fn parse(file: &[u8]) -> Option<Self> {
        // header: magic, version, 15 unused bytes, six counts
        let header = |at: usize| -> Option<(u8, [usize; 6])> {
            let h = file.get(at..at + 44)?;
            if &h[..4] != b"TZif" {
                return None;
            }
            let mut counts = [0usize; 6];
            for (i, count) in counts.iter_mut().enumerate() {
                *count = usize::try_from(be(&h[20 + i * 4..24 + i * 4])).ok()?;
            }
            Some((h[4], counts))
        };
        let size = |[isut, isstd, leap, time, types, chars]: [usize; 6], width: usize| {
            time * width + time + types * 6 + chars + leap * (width + 4) + isstd + isut
        };
        let (version, first) = header(0)?;
        // version 2 and later repeat the data with 64-bit times, followed by the rule for the future
        let (at, counts, width) = if version >= b'2' {
            let second = 44 + size(first, 4);
            (second + 44, header(second)?.1, 8)
        } else {
            (44, first, 4)
        };
        let [_, _, _, times, types, _] = counts;
        let signed = |bytes: &[u8]| {
            if width == 8 { be(bytes) } else { i64::from(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])) }
        };
        let data = file.get(at..at + size(counts, width))?;
        let offsets: Vec<i64> = (0..types)
            .map(|i| {
                let t = &data[times * width + times + i * 6..];
                i64::from(i32::from_be_bytes([t[0], t[1], t[2], t[3]]))
            })
            .collect();
        let mut transitions = Vec::with_capacity(times);
        for i in 0..times {
            let when = signed(&data[i * width..(i + 1) * width]);
            let kind = usize::from(data[times * width + i]);
            transitions.push((when, *offsets.get(kind)?));
        }
        let rule = (version >= b'2')
            .then(|| file.get(at + size(counts, width)..))
            .flatten()
            .and_then(|footer| std::str::from_utf8(footer).ok())
            .and_then(|footer| Rule::parse(footer.trim()));
        Some(Self { transitions, initial: offsets.first().copied().unwrap_or(0), rule })
    }

    /// The zone of this machine: `/usr/share/zoneinfo/<name>` for its configured name, else
    /// `/etc/localtime` itself, else UTC.
    pub fn load(host: &dyn Host) -> Self {
        let named = host::timezone(host).and_then(|name| host.read(&Path::new("/usr/share/zoneinfo").join(name)).ok());
        named
            .or_else(|| host.read(Path::new("/etc/localtime")).ok())
            .and_then(|file| Self::parse(&file))
            .unwrap_or_else(Self::utc)
    }

    /// Seconds east of UTC at this moment.
    pub fn offset_at(&self, utc: i64) -> i64 {
        match self.transitions.last() {
            None => self.rule.as_ref().map_or(self.initial, |rule| rule.offset_at(utc)),
            Some((last, offset)) if utc >= *last => self.rule.as_ref().map_or(*offset, |rule| rule.offset_at(utc)),
            Some(_) => {
                let index = self.transitions.partition_point(|(when, _)| *when <= utc);
                if index == 0 { self.initial } else { self.transitions[index - 1].1 }
            }
        }
    }

    /// Local wall-clock seconds for a moment.
    pub fn to_local(&self, utc: i64) -> i64 {
        utc + self.offset_at(utc)
    }

    /// The moment a local wall-clock time happens. A time that happens twice (clocks set back) is its
    /// first occurrence; one that never happens (clocks set forward) is taken as if they had not been
    /// set yet (02:30 on that night is an hour after 01:30), which is where systemd puts it too.
    pub fn to_utc(&self, local: i64) -> i64 {
        let (before, after) = (self.offset_at(local - 86_400), self.offset_at(local + 86_400));
        let mut valid: Vec<i64> =
            [before, after].iter().map(|offset| local - offset).filter(|utc| self.to_local(*utc) == local).collect();
        valid.sort_unstable();
        valid.first().copied().unwrap_or(local - before)
    }
}
