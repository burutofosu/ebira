//! Time, in one place. A timestamp names an instant. One written without a zone is local
//! time at the corpus offset, everywhere: when records are ordered, filtered, and put on
//! dates. A `--from`/`--to` bound is a date, meaning that whole local day, or a time, meaning
//! that instant.

use std::cmp::Ordering;
use std::io;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// A calendar date written `yyyy-mm-dd`, `yyyy/mm/dd`, or `mm/dd/yyyy`.
pub fn parse_date(text: &str) -> Option<(i64, u32, u32)> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 {
        return None;
    }
    let (year, month, day) = match (bytes[2], bytes[4], bytes[5], bytes[7]) {
        (_, b'-', _, b'-') | (_, b'/', _, b'/') => (
            fixed(text.get(..4)?)? as i64,
            fixed(text.get(5..7)?)?,
            fixed(text.get(8..10)?)?,
        ),
        (b'/', _, b'/', _) => (
            fixed(text.get(6..10)?)? as i64,
            fixed(text.get(..2)?)?,
            fixed(text.get(3..5)?)?,
        ),
        _ => return None,
    };
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some((year, month, day))
}

/// A UTC offset written `Z`, `+hh`, `+hhmm`, or `+hh:mm` (or with `-`), in minutes east of UTC.
pub fn parse_offset(text: &str) -> Option<i64> {
    if text.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = text[1..].replacen(':', "", 1);
    if digits.len() != 2 && digits.len() != 4 {
        return None;
    }
    let hours = i64::from(fixed(digits.get(..2)?)?);
    let minutes = match digits.get(2..) {
        Some("") | None => 0,
        Some(text) => i64::from(fixed(text)?),
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

pub fn format_offset(minutes: i64) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    format!(
        "{}{:02}:{:02}",
        sign,
        minutes.abs() / 60,
        minutes.abs() % 60
    )
}

/// `EBIRA_TZ_OFFSET`, the offset a corpus is built with; UTC when it is unset or empty.
pub fn configured_offset_minutes() -> io::Result<i64> {
    let Ok(value) = std::env::var("EBIRA_TZ_OFFSET") else {
        return Ok(0);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(0);
    }
    parse_offset(value).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "EBIRA_TZ_OFFSET is not a UTC offset such as +09:00: {}",
                value
            ),
        )
    })
}

/// The instant a timestamp names, in nanoseconds since 1970-01-01T00:00:00Z. A timestamp
/// without a zone is local time at `offset_minutes`, and a bare date is the start of that
/// local day.
pub fn instant(timestamp: &str, offset_minutes: i64) -> Option<i128> {
    let timestamp = timestamp.trim();
    let (year, month, day) = parse_date(timestamp.get(..10)?)?;
    let days = i128::from(days_from_civil(year, month, day));
    let rest = &timestamp[10..];
    let (clock_seconds, fraction, zone) = if rest.is_empty() {
        (0, 0, None)
    } else {
        let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
        let zone_start = rest.char_indices().skip(1).find_map(|(index, character)| {
            matches!(character, 'Z' | 'z' | '+' | '-').then_some(index)
        });
        let (clock, zone) = match zone_start {
            Some(index) => (&rest[..index], Some(parse_offset(&rest[index..])?)),
            None => (rest, None),
        };
        let mut parts = clock.split(':');
        let hour = fixed(parts.next()?)?;
        let minute = fixed(parts.next()?)?;
        let second_text = parts.next().unwrap_or("0");
        if parts.next().is_some() {
            return None;
        }
        let (second_text, fraction_text) = match second_text.split_once('.') {
            Some((second, fraction)) => (second, Some(fraction)),
            None => (second_text, None),
        };
        let second = fixed(second_text)?;
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        let fraction = match fraction_text {
            Some(text) => fraction_nanos(text)?,
            None => 0,
        };
        (
            i128::from(hour) * 3_600 + i128::from(minute) * 60 + i128::from(second),
            fraction,
            zone,
        )
    };
    let offset_seconds = i128::from(zone.unwrap_or(offset_minutes)) * 60;
    Some((days * 86_400 + clock_seconds - offset_seconds) * NANOS_PER_SECOND + fraction)
}

/// The local date of a timestamp at `offset_minutes`, as `yyyy-mm-dd`.
pub fn local_date(timestamp: &str, offset_minutes: i64) -> Option<String> {
    let (year, month, day, ..) = local_civil(timestamp, offset_minutes)?;
    Some(format!("{:04}-{:02}-{:02}", year, month, day))
}

/// The date bucket of a record: its local date, or `undated`.
pub fn date_bucket(timestamp: &str, offset_minutes: i64) -> String {
    local_date(timestamp, offset_minutes).unwrap_or_else(|| "undated".to_string())
}

/// `yyyy-mm-dd hh:mm` local time, for listings read by people.
pub fn local_minute(timestamp: &str, offset_minutes: i64) -> Option<String> {
    let (year, month, day, hour, minute, _) = local_civil(timestamp, offset_minutes)?;
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        year, month, day, hour, minute
    ))
}

fn local_civil(timestamp: &str, offset_minutes: i64) -> Option<(i64, u32, u32, u32, u32, u32)> {
    let nanos = instant(timestamp, offset_minutes)?;
    let unix_ms = (nanos.div_euclid(1_000_000)).clamp(i128::from(i64::MIN), i128::from(i64::MAX));
    Some(civil_from_unix_ms(unix_ms as i64, offset_minutes))
}

/// Orders timestamps by the instant they name; those that cannot be read come last, in text
/// order.
pub fn compare(left: &str, right: &str, offset_minutes: i64) -> Ordering {
    match (
        instant(left, offset_minutes),
        instant(right, offset_minutes),
    ) {
        (Some(left_value), Some(right_value)) => {
            left_value.cmp(&right_value).then_with(|| left.cmp(right))
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

/// One end of a `--from`/`--to` range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bound {
    /// A whole local day, `yyyy-mm-dd`.
    Day(String),
    /// An instant, in nanoseconds since 1970 UTC.
    At(i128),
}

impl Bound {
    pub fn parse(text: &str, offset_minutes: i64) -> Option<Bound> {
        let text = text.trim();
        if text.len() == 10 {
            let (year, month, day) = parse_date(text)?;
            return Some(Bound::Day(format!("{:04}-{:02}-{:02}", year, month, day)));
        }
        instant(text, offset_minutes).map(Bound::At)
    }

    fn day(&self, offset_minutes: i64) -> String {
        match self {
            Bound::Day(day) => day.clone(),
            Bound::At(nanos) => {
                let unix_ms = nanos.div_euclid(1_000_000) as i64;
                let (year, month, day, ..) = civil_from_unix_ms(unix_ms, offset_minutes);
                format!("{:04}-{:02}-{:02}", year, month, day)
            }
        }
    }
}

/// A `--from`/`--to` range. A timestamp that cannot be read is outside any bounded range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub from: Option<Bound>,
    pub to: Option<Bound>,
}

impl Range {
    pub fn parse(from: Option<&str>, to: Option<&str>, offset_minutes: i64) -> io::Result<Range> {
        let bound = |name: &str, text: Option<&str>| -> io::Result<Option<Bound>> {
            let Some(text) = text else {
                return Ok(None);
            };
            Bound::parse(text, offset_minutes).map(Some).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} must be a date (yyyy-mm-dd) or a time (yyyy-mm-ddThh:mm:ss, with or without a zone), got {}",
                        name, text
                    ),
                )
            })
        };
        Ok(Range {
            from: bound("--from", from)?,
            to: bound("--to", to)?,
        })
    }

    pub fn is_unbounded(&self) -> bool {
        self.from.is_none() && self.to.is_none()
    }

    pub fn contains(&self, timestamp: &str, offset_minutes: i64) -> bool {
        if self.is_unbounded() {
            return true;
        }
        let Some(at) = instant(timestamp, offset_minutes) else {
            return false;
        };
        let day = || local_date(timestamp, offset_minutes).unwrap_or_default();
        let after_from = match &self.from {
            None => true,
            Some(Bound::Day(from)) => day() >= *from,
            Some(Bound::At(from)) => at >= *from,
        };
        let before_to = match &self.to {
            None => true,
            Some(Bound::Day(to)) => day() <= *to,
            Some(Bound::At(to)) => at <= *to,
        };
        after_from && before_to
    }

    /// Whether a local date, `yyyy-mm-dd`, overlaps the range.
    pub fn touches_date(&self, date: &str, offset_minutes: i64) -> bool {
        let after_from = self
            .from
            .as_ref()
            .is_none_or(|from| date >= from.day(offset_minutes).as_str());
        let before_to = self
            .to
            .as_ref()
            .is_none_or(|to| date <= to.day(offset_minutes).as_str());
        after_from && before_to
    }
}

pub fn civil_from_unix_ms(unix_ms: i64, offset_minutes: i64) -> (i64, u32, u32, u32, u32, u32) {
    let shifted = unix_ms + offset_minutes * 60_000;
    let seconds = shifted.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (second_of_day / 3600) as u32,
        ((second_of_day % 3600) / 60) as u32,
        (second_of_day % 60) as u32,
    )
}

pub fn rfc3339_from_unix_ms(unix_ms: i64, offset_minutes: i64) -> String {
    let (year, month, day, hour, minute, second) = civil_from_unix_ms(unix_ms, offset_minutes);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}",
        year,
        month,
        day,
        hour,
        minute,
        second,
        format_offset(offset_minutes)
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    }
    .div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn fixed(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn fraction_nanos(text: &str) -> Option<i128> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let prefix = &text[..text.len().min(9)];
    let value = prefix.parse::<i128>().ok()?;
    Some(value * 10i128.pow(9 - prefix.len() as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_time_round_trips_offsets() {
        assert_eq!(civil_from_unix_ms(0, 0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(rfc3339_from_unix_ms(0, 0), "1970-01-01T00:00:00+00:00");
        let instant = 1_787_011_200_000;
        assert_eq!(civil_from_unix_ms(instant, 0), (2026, 8, 18, 0, 0, 0));
        assert_eq!(civil_from_unix_ms(instant, 9 * 60), (2026, 8, 18, 9, 0, 0));
        assert_eq!(
            rfc3339_from_unix_ms(instant, -5 * 60),
            "2026-08-17T19:00:00-05:00"
        );
        assert_eq!(
            civil_from_unix_ms(1_709_164_800_000, 0),
            (2024, 2, 29, 0, 0, 0)
        );
        assert_eq!(civil_from_unix_ms(-1, 0), (1969, 12, 31, 23, 59, 59));
    }

    #[test]
    fn one_date_grammar_and_one_offset_grammar() {
        for text in ["2026-02-22", "2026/02/22", "02/22/2026"] {
            assert_eq!(parse_date(text), Some((2026, 2, 22)), "{text}");
        }
        assert_eq!(parse_date("2026-02-30"), None);
        for (text, minutes) in [
            ("Z", 0),
            ("+09:00", 540),
            ("+0900", 540),
            ("+09", 540),
            ("-05:30", -330),
        ] {
            assert_eq!(parse_offset(text), Some(minutes), "{text}");
        }
        assert_eq!(parse_offset("09:00"), None);
        assert_eq!(parse_offset("+24:00"), None);
    }

    #[test]
    fn a_timestamp_without_a_zone_is_local_time_everywhere() {
        let offset = 9 * 60;
        let zoneless = "2026-08-18T20:00:00";
        let zoned = "2026-08-18T20:00:00Z";
        assert_eq!(date_bucket(zoneless, offset), "2026-08-18");
        assert_eq!(date_bucket(zoned, offset), "2026-08-19");
        // Ordering agrees with dates: local 20:00 on the 18th is before 20:00Z the same day.
        assert_eq!(compare(zoneless, zoned, offset), Ordering::Less);
        assert_eq!(
            instant(zoneless, offset),
            instant("2026-08-18T11:00:00Z", 0)
        );
        assert_eq!(
            compare("2026-02-22T00:00:00+09:00", "2026-02-21T16:00:00Z", 0),
            Ordering::Less
        );
        assert_eq!(date_bucket("2026/02/22 10:21:39", 0), "2026-02-22");
        assert_eq!(date_bucket("02/22/2026 10:21:39", 0), "2026-02-22");
        assert_eq!(date_bucket("not-a-date", 0), "undated");
        // A time that cannot be read leaves the record undated, as it leaves it out of a range.
        assert_eq!(date_bucket("2026-02-22T25:00:00Z", 0), "undated");
    }

    #[test]
    fn a_range_is_whole_local_days_or_instants() {
        let offset = 9 * 60;
        let day = Range::parse(Some("2026-08-18"), Some("2026-08-18"), offset).expect("range");
        assert!(day.contains("2026-08-18T00:00:00", offset));
        assert!(
            day.contains("2026-08-18T14:59:59Z", offset),
            "23:59:59 local"
        );
        assert!(
            !day.contains("2026-08-18T15:00:00Z", offset),
            "the next local day"
        );
        assert!(!day.contains("not a time", offset));
        let instants =
            Range::parse(Some("2026-08-18T00:00:00+09:00"), None, offset).expect("range");
        assert!(instants.contains("2026-08-18T00:00:00", offset));
        assert!(!instants.contains("2026-08-17T23:59:59", offset));
        assert!(day.touches_date("2026-08-18", offset));
        assert!(!day.touches_date("2026-08-19", offset));
        assert!(instants.touches_date("2026-08-18", offset));
        assert!(Range::parse(Some("yesterday"), None, offset).is_err());
        assert!(Range::default().contains("not a time", offset));
    }
}
