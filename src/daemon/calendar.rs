//! Calendar schedules: the subset of systemd's `OnCalendar` syntax moi uses — `[weekdays] [date] time`
//! and the shorthands (`daily`, `weekly`, …) — and when one is next due, in the server's local time.

use super::zone::Zone;
use crate::host::{civil_from_days, days_from_civil};

/// One comma-separated alternative of a field: `*`, `5`, `1..5`, each optionally `/step`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Alternative {
    /// `None` for `*`.
    from: Option<i64>,
    to: Option<i64>,
    step: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field(Vec<Alternative>);

impl Field {
    fn parse(text: &str) -> Option<Self> {
        let mut alternatives = Vec::new();
        for alt in text.split(',') {
            let (range, step) = match alt.split_once('/') {
                Some((range, step)) => (range, Some(step.parse::<i64>().ok().filter(|s| *s > 0)?)),
                None => (alt, None),
            };
            let (from, to) = match range.split_once("..") {
                Some((from, to)) => (Some(from.parse().ok()?), Some(to.parse().ok()?)),
                None if range == "*" => (None, None),
                None => (Some(range.parse().ok()?), None),
            };
            alternatives.push(Alternative { from, to, step });
        }
        Some(Self(alternatives))
    }

    fn any() -> Self {
        Self(vec![Alternative { from: None, to: None, step: None }])
    }

    fn exactly(value: i64) -> Self {
        Self(vec![Alternative { from: Some(value), to: None, step: None }])
    }

    fn matches(&self, value: i64) -> bool {
        self.0.iter().any(|alt| match (alt.from, alt.to, alt.step) {
            (None, _, None) => true,
            (None, _, Some(step)) => value % step == 0,
            (Some(from), None, None) => value == from,
            // `5/10`: 5, 15, 25, …
            (Some(from), None, Some(step)) => value >= from && (value - from) % step == 0,
            (Some(from), Some(to), step) => (from..=to).contains(&value) && (value - from) % step.unwrap_or(1) == 0,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar {
    /// Weekdays it may fall on, Monday = 0; empty = any.
    weekdays: Vec<i64>,
    year: Field,
    month: Field,
    day: Field,
    hour: Field,
    minute: Field,
    second: Field,
}

const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

fn weekday(name: &str) -> Option<i64> {
    let short = name.get(..3)?.to_ascii_lowercase();
    WEEKDAYS.iter().position(|d| *d == short).and_then(|i| i64::try_from(i).ok())
}

impl Calendar {
    pub fn parse(text: &str) -> Option<Self> {
        let expanded = match text.trim() {
            "minutely" => "*-*-* *:*:00",
            "hourly" => "*-*-* *:00:00",
            "daily" => "*-*-* 00:00:00",
            "weekly" => "Mon *-*-* 00:00:00",
            "monthly" => "*-*-01 00:00:00",
            "quarterly" => "*-01,04,07,10-01 00:00:00",
            "yearly" | "annually" => "*-01-01 00:00:00",
            other => other,
        };
        let mut parts: Vec<&str> = expanded.split_whitespace().collect();
        let mut time = parts.pop()?.split(':');
        let (hour, minute) = (Field::parse(time.next()?)?, Field::parse(time.next()?)?);
        let second = time.next().map_or_else(|| Some(Field::exactly(0)), Field::parse)?;
        if time.next().is_some() {
            return None;
        }
        let (mut year, mut month, mut day) = (Field::any(), Field::any(), Field::any());
        if parts.last().is_some_and(|p| p.contains('-') && !p.chars().any(|c| c.is_ascii_alphabetic())) {
            let date: Vec<&str> = parts.pop()?.split('-').collect();
            match date.as_slice() {
                [y, m, d] => (year, month, day) = (Field::parse(y)?, Field::parse(m)?, Field::parse(d)?),
                [m, d] => (month, day) = (Field::parse(m)?, Field::parse(d)?),
                _ => return None,
            }
        }
        let mut weekdays = Vec::new();
        match parts.as_slice() {
            [] => {}
            [days] => {
                for item in days.split(',') {
                    match item.split_once("..") {
                        Some((from, to)) => {
                            let (from, to) = (weekday(from)?, weekday(to)?);
                            // `Fri..Mon` wraps around the week
                            let mut day = from;
                            loop {
                                weekdays.push(day);
                                if day == to {
                                    break;
                                }
                                day = (day + 1) % 7;
                            }
                        }
                        None => weekdays.push(weekday(item)?),
                    }
                }
            }
            _ => return None,
        }
        Some(Self { weekdays, year, month, day, hour, minute, second })
    }

    /// The first moment after `after` (seconds since the epoch) the schedule is due, if there is one
    /// within the next eight years.
    pub fn next_after(&self, after: i64, zone: &Zone) -> Option<i64> {
        let local = zone.to_local(after);
        let first_day = local.div_euclid(86_400);
        for day in first_day..first_day + 8 * 366 {
            let (y, m, d) = civil_from_days(day);
            // 1970-01-01 was a Thursday (3 with Monday = 0)
            let on_weekday = self.weekdays.is_empty() || self.weekdays.contains(&(day + 3).rem_euclid(7));
            if !(on_weekday && self.year.matches(y) && self.month.matches(m) && self.day.matches(d)) {
                continue;
            }
            debug_assert_eq!(days_from_civil(y, m, d), day);
            for hour in (0..24).filter(|h| self.hour.matches(*h)) {
                for minute in (0..60).filter(|m| self.minute.matches(*m)) {
                    for second in (0..60).filter(|s| self.second.matches(*s)) {
                        let moment = zone.to_utc(day * 86_400 + hour * 3600 + minute * 60 + second);
                        if moment > after {
                            return Some(moment);
                        }
                    }
                }
            }
        }
        None
    }
}
