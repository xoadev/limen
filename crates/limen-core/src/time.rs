//! UTC timestamps as `2026-09-26T08:00:00Z`, without a date library: civil dates from days since the epoch.

const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_HOUR: i64 = 3600;
const SECONDS_PER_MINUTE: i64 = 60;

/// Seconds since the epoch as `2026-09-26T08:00:00Z`.
pub fn iso(epoch_seconds: i64) -> String {
    let (year, month, day) = civil_from_days(epoch_seconds.div_euclid(SECONDS_PER_DAY));
    let second_of_day = epoch_seconds.rem_euclid(SECONDS_PER_DAY);
    let hour = second_of_day / SECONDS_PER_HOUR;
    let minute = second_of_day % SECONDS_PER_HOUR / SECONDS_PER_MINUTE;
    let second = second_of_day % SECONDS_PER_MINUTE;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// `2026-09-26T08:00:00Z`, `2026-09-26T08:00Z` or with fractional seconds: seconds since the epoch.
pub fn parse_iso(text: &str) -> Option<i64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let (year, month, day) = parse_date(date)?;
    let second_of_day = parse_time_of_day(time)?;
    Some(days_from_civil(year, month, day) * SECONDS_PER_DAY + second_of_day)
}

/// `2026-09-26`: year, month and day.
fn parse_date(date: &str) -> Option<(i64, i64, i64)> {
    let mut parts = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// `08:00:00`, `08:00` or `08:00:00.123`: seconds since midnight, the fraction dropped.
fn parse_time_of_day(time: &str) -> Option<i64> {
    let mut parts = time.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let second: i64 = match parts.next() {
        Some(seconds) => seconds.split('.').next()?.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(hour * SECONDS_PER_HOUR + minute * SECONDS_PER_MINUTE + second)
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_secs() as i64)
        .unwrap_or(0)
}

// Howard Hinnant's algorithms: exact for the proleptic Gregorian calendar. Their years start on March 1st, so that
// February, and its leap day, comes last.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 { month_from_march + 3 } else { month_from_march - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_from_march = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_790_409_600), "2026-09-26T08:00:00Z");
        assert_eq!(parse_iso("2026-09-26T08:00:00Z"), Some(1_790_409_600));
        assert_eq!(parse_iso("2026-09-26T08:00Z"), Some(1_790_409_600));
        assert_eq!(parse_iso("2026-09-26T08:00:00.123456789Z"), Some(1_790_409_600));
        assert_eq!(parse_iso("2024-02-29T23:59:59Z").map(iso).as_deref(), Some("2024-02-29T23:59:59Z"));
        assert_eq!(parse_iso("2026-13-01T00:00Z"), None);
        assert_eq!(parse_iso("yesterday"), None);
    }
}
