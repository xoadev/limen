//! UTC timestamps as `2026-09-26T08:00:00Z`, without a date library: civil dates from days since the epoch.

/// Seconds since the epoch as `2026-09-26T08:00:00Z`.
pub fn iso(epoch_seconds: i64) -> String {
    let days = epoch_seconds.div_euclid(86_400);
    let secs = epoch_seconds.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// `2026-09-26T08:00:00Z`, `2026-09-26T08:00Z` or with fractional seconds: seconds since the epoch.
pub fn parse_iso(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    if d.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let s: i64 = match t.next() {
        Some(s) => s.split('.').next()?.parse().ok()?,
        None => 0,
    };
    if t.next().is_some() || h > 23 || min > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(y, m, day) * 86_400 + h * 3600 + min * 60 + s)
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// Howard Hinnant's algorithms: exact for the proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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
