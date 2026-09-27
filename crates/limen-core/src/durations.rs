//! `500ms`, `30s`, `5m`, `1h`, `2d`: how every duration in limen's files and arguments is written.

use std::time::Duration;

pub fn parse(text: &str) -> Option<Duration> {
    let text = text.trim();
    let unit_at = text.find(|c: char| !c.is_ascii_digit())?;
    let (digits, unit) = text.split_at(unit_at);
    if digits.is_empty() || digits.len() > 9 {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n * 60)),
        "h" => Some(Duration::from_secs(n * 3600)),
        "d" => Some(Duration::from_secs(n * 86_400)),
        _ => None,
    }
}

/// How a duration is shown back: `60s`, `1h`, the largest unit that divides it.
pub fn show(d: Duration) -> String {
    let s = d.as_secs();
    if d.subsec_millis() != 0 || s == 0 {
        format!("{}ms", d.as_millis())
    } else if s % 86_400 == 0 {
        format!("{}d", s / 86_400)
    } else if s % 3600 == 0 {
        format!("{}h", s / 3600)
    } else if s % 60 == 0 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_and_shapes() {
        assert_eq!(parse("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse(" 2h "), Some(Duration::from_secs(7200)));
        assert_eq!(parse("1d"), Some(Duration::from_secs(86_400)));
        for bad in ["", "s", "5", "5x", "-1s", "1.5s", "1234567890s"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(show(Duration::from_secs(60)), "1m");
        assert_eq!(show(Duration::from_secs(90)), "90s");
    }
}
