use std::time::Duration;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const MIN_RATE: u64 = KIB;

pub fn parse_duration(text: &str) -> Option<Duration> {
    let unit = match text.chars().last()? {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        'w' => 7 * 86_400,
        'y' => 365 * 86_400,
        _ => return None,
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs = digits.parse::<u64>().ok()?.checked_mul(unit)?;
    Some(Duration::from_secs(secs))
}

pub fn parse_size(text: &str) -> Option<u64> {
    let (digits, unit) = [("GiB", GIB), ("MiB", MIB), ("KiB", KIB), ("B", 1)]
        .into_iter()
        .find_map(|(suffix, unit)| text.strip_suffix(suffix).map(|d| (d, unit)))?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits
        .parse::<u64>()
        .ok()?
        .checked_mul(unit)
        .filter(|size| *size > 0)
}

pub fn parse_rate(text: &str) -> Option<u64> {
    let rate = parse_size(text.strip_suffix("/s")?)?;
    (rate >= MIN_RATE).then_some(rate)
}

pub fn format_size(bytes: u64) -> String {
    let (unit, name) = [(GIB, "GiB"), (MIB, "MiB"), (KIB, "KiB")]
        .into_iter()
        .find(|(unit, _)| bytes > 0 && bytes.is_multiple_of(*unit))
        .unwrap_or((1, "B"));
    format!("{}{name}", bytes / unit)
}

pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let whole = |unit: u64| secs > 0 && secs.is_multiple_of(unit);
    if whole(86_400) && secs >= 2 * 86_400 {
        format!("{}d", secs / 86_400)
    } else if whole(3600) {
        format!("{}h", secs / 3600)
    } else if whole(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_parses_binary_suffixes_only() {
        assert_eq!(parse_size("10GiB"), Some(10 << 30));
        assert_eq!(parse_size("500MiB"), Some(500 << 20));
        assert_eq!(parse_size("1KiB"), Some(1024));
        assert_eq!(parse_size("0B"), None);
        assert_eq!(parse_size("10GB"), None);
        assert_eq!(parse_size("GiB"), None);
        assert_eq!(parse_size("1.5GiB"), None);
    }

    #[test]
    fn size_formats_the_largest_exact_unit() {
        assert_eq!(format_size(10 << 30), "10GiB");
        assert_eq!(format_size(1536 << 20), "1536MiB");
        assert_eq!(format_size(1000), "1000B");
        assert_eq!(parse_size(&format_size(7 << 20)), Some(7 << 20));
    }

    #[test]
    fn format_duration_prefers_days_then_hours() {
        assert_eq!(format_duration(Duration::from_secs(31 * 3600)), "31h");
        assert_eq!(format_duration(Duration::from_secs(48 * 3600)), "2d");
        assert_eq!(format_duration(Duration::from_secs(72 * 3600)), "3d");
        assert_eq!(format_duration(Duration::from_secs(24 * 3600)), "24h");
        assert_eq!(format_duration(Duration::from_secs(90 * 60)), "90m");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(parse_duration("31h"), Some(Duration::from_secs(31 * 3600)));
    }
}
