//! Human-readable number formatting.

use std::time::Duration;

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// `+1.2 MiB`, `-512 B`.
pub fn signed_bytes(delta: i64) -> String {
    let sign = if delta < 0 { '-' } else { '+' };
    format!("{sign}{}", bytes(delta.unsigned_abs()))
}

/// `HH:MM:SS`, prefixed with days when needed.
pub fn duration(secs: u64) -> String {
    let (days, h, m, s) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60, secs % 60);
    if days > 0 {
        format!("{days}d {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// Compact count: `950`, `12.3k`, `4.1M`.
pub fn count(value: f64) -> String {
    match value {
        v if v >= 1e6 => format!("{:.1}M", v / 1e6),
        v if v >= 1e3 => format!("{:.1}k", v / 1e3),
        v => format!("{v:.0}"),
    }
}

pub fn interval(d: Duration) -> String {
    let ms = d.as_millis();
    match ms {
        ms if ms < 1_000 => format!("{ms}ms"),
        ms if ms % 1_000 == 0 => format!("{}s", ms / 1_000),
        _ => format!("{:.1}s", d.as_secs_f64()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(signed_bytes(-1536), "-1.5 KiB");
        assert_eq!(signed_bytes(0), "+0 B");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration(59), "00:00:59");
        assert_eq!(duration(3_661), "01:01:01");
        assert_eq!(duration(90_061), "1d 01:01:01");
    }

    #[test]
    fn formats_intervals() {
        assert_eq!(interval(Duration::from_millis(250)), "250ms");
        assert_eq!(interval(Duration::from_millis(2_000)), "2s");
        assert_eq!(interval(Duration::from_millis(1_500)), "1.5s");
    }
}
