//! Text formatting for pages and JSON.

use chrono::{DateTime, SecondsFormat, Utc};

/// Binary size units.
const SIZE_UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
const SIZE_STEP: u128 = 1024;
/// Tenths at which a value is shown in the next unit (1023.95 → 1.0).
const NEXT_UNIT_TENTHS: u128 = SIZE_STEP * 10;

/// A size in binary units with one decimal, e.g. `1.5 GiB`; bytes below
/// 1 KiB are shown exactly, e.g. `512 B`.
pub(crate) fn human_size(bytes: u64) -> String {
    let value = u128::from(bytes);
    if value < SIZE_STEP {
        return format!("{bytes} B");
    }
    let mut unit = 0usize;
    let mut scale: u128 = 1;
    let last = SIZE_UNITS.len().saturating_sub(1);
    while unit < last && value >= scale.saturating_mul(SIZE_STEP) {
        scale = scale.saturating_mul(SIZE_STEP);
        unit = unit.saturating_add(1);
    }
    let mut tenths = rounded_tenths(value, scale);
    if tenths >= NEXT_UNIT_TENTHS && unit < last {
        scale = scale.saturating_mul(SIZE_STEP);
        unit = unit.saturating_add(1);
        tenths = rounded_tenths(value, scale);
    }
    let name = SIZE_UNITS.get(unit).copied().unwrap_or("B");
    format!(
        "{}.{} {name}",
        tenths.checked_div(10).unwrap_or(0),
        tenths.checked_rem(10).unwrap_or(0)
    )
}

/// `value / scale` in tenths, rounded half up. `value` is at most
/// `u64::MAX`, so `value * 10` fits in `u128`.
fn rounded_tenths(value: u128, scale: u128) -> u128 {
    let half = scale.checked_div(2).unwrap_or(0);
    value
        .saturating_mul(10)
        .saturating_add(half)
        .checked_div(scale)
        .unwrap_or(0)
}

/// An integer with thousands separators, e.g. `1,234,567`.
pub(crate) fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len().saturating_add(digits.len() / 3));
    let len = digits.len();
    for (i, c) in digits.chars().enumerate() {
        let remaining = len.saturating_sub(i);
        if i > 0 && remaining % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A signed count for display; negative values (which the store never
/// returns) show as 0.
pub(crate) fn grouped_signed(n: i64) -> String {
    grouped(u64::try_from(n).unwrap_or(0))
}

/// `1 file`, `2 files`.
pub(crate) fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", grouped(n))
    }
}

/// The UTC calendar date, e.g. `2026-09-17`.
pub(crate) fn date(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d").to_string()
}

/// RFC 3339 with whole seconds, e.g. `2026-09-17T08:15:00Z`.
pub(crate) fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(1_048_575), "1.0 MiB");
        assert_eq!(human_size(1_073_741_824), "1.0 GiB");
        assert_eq!(human_size(4_700_000_000), "4.4 GiB");
        assert_eq!(human_size(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn grouping() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(grouped(u64::MAX), "18,446,744,073,709,551,615");
        assert_eq!(grouped_signed(-5), "0");
        assert_eq!(grouped_signed(12_345), "12,345");
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "file", "files"), "1 file");
        assert_eq!(plural(0, "file", "files"), "0 files");
        assert_eq!(plural(2500, "time", "times"), "2,500 times");
    }

    #[test]
    fn dates() {
        let t = Utc.with_ymd_and_hms(2026, 9, 17, 8, 15, 0).unwrap();
        assert_eq!(date(t), "2026-09-17");
        assert_eq!(rfc3339(t), "2026-09-17T08:15:00Z");
    }
}
