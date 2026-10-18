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

pub fn parse_rate(text: &str) -> Option<u64> {
    let body = text.strip_suffix("/s")?;
    let (digits, unit) = [("GiB", GIB), ("MiB", MIB), ("KiB", KIB), ("B", 1)]
        .into_iter()
        .find_map(|(suffix, unit)| body.strip_suffix(suffix).map(|d| (d, unit)))?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rate = digits.parse::<u64>().ok()?.checked_mul(unit)?;
    (rate >= MIN_RATE).then_some(rate)
}
