//! Value types of the config: durations (`"5m"`), sizes (`"20GB"`), secrets, `"auto"`-or-list.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Serialize, Serializer};

use crate::redact::MASK;

/// A duration written as a number and a unit: `30s`, `5m`, `1h`, `7d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dur(u64);

impl Dur {
    pub const fn from_secs(secs: u64) -> Self {
        Self(secs)
    }

    pub const fn as_secs(self) -> u64 {
        self.0
    }

    pub const fn as_duration(self) -> Duration {
        Duration::from_secs(self.0)
    }
}

const DUR_UNITS: [(&str, u64); 4] = [("d", 86_400), ("h", 3600), ("m", 60), ("s", 1)];

impl FromStr for Dur {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let err = || {
            format!(
                "invalid duration \"{s}\": use a number followed by s, m, h or d (e.g. \"30s\", \"5m\", \"20h\", \"7d\")"
            )
        };
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        let (number, unit) = s.split_at(digits);
        let n: u64 = number.parse().map_err(|_| err())?;
        let (_, factor) = DUR_UNITS.iter().find(|(u, _)| *u == unit).ok_or_else(err)?;
        n.checked_mul(*factor).map(Self).ok_or_else(err)
    }
}

impl fmt::Display for Dur {
    /// The largest unit that divides the value exactly, so parsing and printing round-trip.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (unit, factor) =
            DUR_UNITS.iter().find(|(_, factor)| self.0 != 0 && self.0.is_multiple_of(*factor)).unwrap_or(&("s", 1));
        write!(f, "{}{unit}", self.0 / factor)
    }
}

impl Serialize for Dur {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// A size in bytes written with a binary unit: `512KB`, `20GB` (powers of 1024).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Size(u64);

impl Size {
    pub const fn from_bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn bytes(self) -> u64 {
        self.0
    }
}

const SIZE_UNITS: [(&str, u64); 5] = [("TB", 1 << 40), ("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10), ("B", 1)];

impl FromStr for Size {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let err =
            || format!("invalid size \"{s}\": use a number followed by B, KB, MB, GB or TB (e.g. \"512KB\", \"20GB\")");
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        let (number, unit) = s.split_at(digits);
        let n: u64 = number.parse().map_err(|_| err())?;
        let (_, factor) = SIZE_UNITS.iter().find(|(u, _)| *u == unit).ok_or_else(err)?;
        n.checked_mul(*factor).map(Self).ok_or_else(err)
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (unit, factor) =
            SIZE_UNITS.iter().find(|(_, factor)| self.0 != 0 && self.0.is_multiple_of(*factor)).unwrap_or(&("B", 1));
        write!(f, "{}{unit}", self.0 / factor)
    }
}

impl Serialize for Size {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// A credential. Its value is only reachable through [`Secret::expose`]; `Debug` and serialization
/// always show `***`, so it cannot end up in logs, panics or `moi config check` by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The secret value, for handing to the service it belongs to.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(MASK)
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(MASK)
    }
}

/// A setting that is either discovered (`"auto"`) or an explicit list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoOr<T> {
    Auto,
    List(Vec<T>),
}

impl<T: Serialize> Serialize for AutoOr<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            AutoOr::Auto => s.serialize_str("auto"),
            AutoOr::List(items) => items.serialize(s),
        }
    }
}
