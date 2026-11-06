use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Microseconds since the Unix epoch, UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ts(pub i64);

impl Ts {
    /// Accepts RFC 3339 (`Z` or an offset) and naive ISO-8601, which is taken as UTC.
    pub fn parse(s: &str) -> Option<Ts> {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(Ts(dt.timestamp_micros()));
        }
        let naive = s.strip_suffix('Z').unwrap_or(s);
        NaiveDateTime::parse_from_str(naive, "%Y-%m-%dT%H:%M:%S%.f")
            .ok()
            .map(|n| Ts(n.and_utc().timestamp_micros()))
    }

    pub fn now() -> Ts {
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Ts(d.as_micros() as i64)
    }
}

impl fmt::Display for Ts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match DateTime::<Utc>::from_timestamp_micros(self.0) {
            Some(dt) => f.write_str(&dt.to_rfc3339_opts(SecondsFormat::Micros, true)),
            None => write!(f, "{}us", self.0),
        }
    }
}

impl Serialize for Ts {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Ts {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Ts, D::Error> {
        let s = String::deserialize(d)?;
        Ts::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid timestamp: {s}")))
    }
}
