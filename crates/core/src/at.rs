//! When a ledger entry was made (GitHub ledger spec §1.3): RFC 3339, UTC, to
//! the millisecond, in exactly one spelling — `2026-09-30T12:34:56.789Z`.
//!
//! ⚠ One fixed-width spelling on purpose. Entries are ordered by `at` and
//! then by id (spec §2.5). With one width and one zone, string order IS time
//! order, so the derived `Ord` is correct and no reader parses an offset to
//! sort. `parse` accepts exactly what `from_unix_millis` writes.
//!
//! No clock here: `fl-core` has none. The caller reads the time
//! (`fl_exec::stamp::now`) and hands in the milliseconds.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// 9999-12-31T23:59:59.999Z, the last instant a four-digit year spells.
const MAX_MILLIS: u64 = 253_402_300_799_999;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct At(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`{0}` is not a time fl writes: it must be RFC 3339 in UTC, to the millisecond, such as \
     `2026-09-30T12:34:56.789Z`"
)]
pub struct AtError(String);

impl At {
    /// `ms` milliseconds after the Unix epoch. Past the year 9999 it is held
    /// at that year's last millisecond, which is all RFC 3339 can spell.
    pub fn from_unix_millis(ms: u64) -> Self {
        let ms = ms.min(MAX_MILLIS);
        let secs = ms / 1000;
        let (y, m, d) = civil_from_days((secs / 86_400) as i64);
        let rem = secs % 86_400;
        At(format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60,
            ms % 1000
        ))
    }

    /// The one spelling, and nothing else: no other offset, no other
    /// precision, no date that does not exist.
    pub fn parse(s: &str) -> Result<Self, AtError> {
        let refuse = || AtError(s.to_string());
        let b = s.as_bytes();
        if b.len() != 24 {
            return Err(refuse());
        }
        for (i, c) in b.iter().enumerate() {
            let fits = match i {
                4 | 7 => *c == b'-',
                10 => *c == b'T',
                13 | 16 => *c == b':',
                19 => *c == b'.',
                23 => *c == b'Z',
                _ => c.is_ascii_digit(),
            };
            if !fits {
                return Err(refuse());
            }
        }
        let n = |from: usize, to: usize| -> i64 {
            s[from..to].parse().expect("checked above: ASCII digits")
        };
        let days = days_from_civil(n(0, 4), n(5, 7), n(8, 10));
        let ms = days * 86_400_000
            + n(11, 13) * 3_600_000
            + n(14, 16) * 60_000
            + n(17, 19) * 1000
            + n(20, 23);
        // An impossible date (February 30th), hour 24 or second 60 lands on
        // another instant, and that instant spells differently. A date
        // before 1970 is negative here, wraps to a huge `u64`, is held at
        // the year 9999, and spells differently too.
        let at = At::from_unix_millis(ms as u64);
        if at.0 != s {
            return Err(refuse());
        }
        Ok(at)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`). Any month or day is accepted and lands somewhere;
/// `parse`'s round trip refuses the ones that do not exist.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `z` days after 1970-01-01 (Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

impl fmt::Display for At {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for At {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for At {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        At::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_spell_as_rfc_3339_utc_to_the_millisecond() {
        for (ms, spelled) in [
            (0, "1970-01-01T00:00:00.000Z"),
            (951_782_400_000, "2000-02-29T00:00:00.000Z"),
            (1_727_658_123_004, "2024-09-30T01:02:03.004Z"),
            (MAX_MILLIS, "9999-12-31T23:59:59.999Z"),
        ] {
            assert_eq!(At::from_unix_millis(ms).as_str(), spelled, "{ms}");
        }
        assert_eq!(
            At::from_unix_millis(u64::MAX).as_str(),
            "9999-12-31T23:59:59.999Z",
            "past the last four-digit year it holds at that year's last millisecond"
        );
    }

    #[test]
    fn every_spelling_parses_back_to_itself() {
        for ms in [
            0,
            999,
            86_399_999,
            86_400_000,
            951_868_799_999,
            1_727_658_123_004,
            MAX_MILLIS,
        ] {
            let at = At::from_unix_millis(ms);
            assert_eq!(At::parse(at.as_str()), Ok(at.clone()), "{at}");
        }
    }

    // ⚠ Entries are ordered by `at` (spec §2.5) through the derived `Ord`,
    // which compares the strings. This pins that string order is time order.
    #[test]
    fn string_order_is_time_order() {
        let times: Vec<At> = [0, 999, 1000, 86_400_000, 951_782_400_000, MAX_MILLIS]
            .iter()
            .map(|ms| At::from_unix_millis(*ms))
            .collect();
        for pair in times.windows(2) {
            assert!(pair[0] < pair[1], "{} is not before {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn anything_but_the_one_spelling_is_refused_by_name() {
        for bad in [
            "",
            "2026-09-30T12:34:56Z",
            "2026-09-30T12:34:56.789+00:00",
            "2026-09-30 12:34:56.789Z",
            "2026-09-30T12:34:56.789z",
            "2026-02-30T00:00:00.000Z",
            "2026-13-01T00:00:00.000Z",
            "2026-09-30T24:00:00.000Z",
            "2026-09-30T23:59:60.000Z",
            "1969-12-31T23:59:59.999Z",
        ] {
            let err = At::parse(bad).expect_err(bad);
            assert!(err.to_string().contains(&format!("`{bad}`")), "{err}");
        }
    }

    #[test]
    fn the_wire_form_is_a_plain_string_checked_on_the_way_in() {
        let at: At = serde_json::from_str("\"2026-09-30T12:34:56.789Z\"").unwrap();
        assert_eq!(
            serde_json::to_string(&at).unwrap(),
            "\"2026-09-30T12:34:56.789Z\""
        );
        assert!(serde_json::from_str::<At>("\"2026-09-30T12:34:56Z\"").is_err());
        assert!(serde_json::from_str::<At>("1727658123004").is_err());
    }
}
