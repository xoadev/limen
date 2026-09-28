//! `500ms`, `30s`, `5m`, `1h`, `2d`: how every duration in limen's files and arguments is written.

use std::time::Duration;

/// The units longer than a millisecond, in seconds, largest first: the order [show] tries them in.
const UNITS: [(&str, u64); 4] = [("d", 86_400), ("h", 3600), ("m", 60), ("s", 1)];

pub fn parse(text: &str) -> Option<Duration> {
    let text = text.trim();
    let unit_start = text.find(|character: char| !character.is_ascii_digit())?;
    let (digits, unit) = text.split_at(unit_start);
    if digits.is_empty() || digits.len() > 9 {
        return None;
    }
    let amount: u64 = digits.parse().ok()?;
    if unit == "ms" {
        return Some(Duration::from_millis(amount));
    }
    let (_, unit_seconds) = UNITS.iter().find(|(name, _)| *name == unit)?;
    Some(Duration::from_secs(amount * unit_seconds))
}

/// How a duration is shown back: `60s`, `1h`, the largest unit that divides it.
pub fn show(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if duration.subsec_millis() != 0 || seconds == 0 {
        return format!("{}ms", duration.as_millis());
    }
    let (name, unit_seconds) =
        UNITS.iter().find(|(_, unit_seconds)| seconds % unit_seconds == 0).expect("a second divides any whole seconds");
    format!("{}{name}", seconds / unit_seconds)
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
