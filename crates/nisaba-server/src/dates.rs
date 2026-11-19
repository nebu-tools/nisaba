//! Calendar dates for `datetime.today()` in an export.

/// A calendar date: year, month, day.
pub type Date = (i32, u8, u8);

/// Days since 1970-01-01 in UTC, from the system clock.
#[must_use]
pub fn utc_days() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    i64::try_from(secs / 86_400).unwrap_or(0)
}

/// The date an export is stamped with.
///
/// The date is the calendar date of the person who asks. The request carries
/// it as `YYYY-MM-DD`. The server cannot check a time zone, so it takes the
/// date when it lies within one day of its own UTC date, which covers every
/// time zone on Earth, and otherwise uses the UTC date.
#[must_use]
pub fn export_date(asked: Option<&str>, utc_days: i64) -> Date {
    asked
        .and_then(parse_date)
        .filter(|&(y, m, d)| (days_from_civil(y, m, d) - utc_days).abs() <= 1)
        .unwrap_or_else(|| civil_from_days(utc_days))
}

fn parse_date(s: &str) -> Option<Date> {
    let mut parts = s.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let date = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    // Round-tripping through day numbers refuses dates like 2027-02-30.
    (civil_from_days(days_from_civil(date.0, date.1, date.2)) == date).then_some(date)
}

/// Converts a calendar date to days since 1970-01-01 (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Converts days since 1970-01-01 to a calendar date (Howard Hinnant's
/// `civil_from_days`).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn civil_from_days(days: i64) -> Date {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year as i32, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_from_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_819), (2027, 1, 1));
        for days in [-1, 0, 59, 11_016, 20_819, 400_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn the_asked_date_counts_within_one_day() {
        let today = days_from_civil(2026, 11, 18);
        assert_eq!(export_date(Some("2026-11-19"), today), (2026, 11, 19));
        assert_eq!(export_date(Some("2026-11-17"), today), (2026, 11, 17));
        assert_eq!(export_date(Some("2026-11-18"), today), (2026, 11, 18));
        for refused in [
            "2026-11-20",
            "2026-11-16",
            "1999-01-01",
            "2026-02-30",
            "18.11.2026",
            "x",
        ] {
            assert_eq!(
                export_date(Some(refused), today),
                (2026, 11, 18),
                "{refused}"
            );
        }
        assert_eq!(export_date(None, today), (2026, 11, 18));
        let new_year = days_from_civil(2027, 1, 1);
        assert_eq!(export_date(Some("2026-12-31"), new_year), (2026, 12, 31));
    }
}
