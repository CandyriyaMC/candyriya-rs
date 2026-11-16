//! Human readable rendering of durations and byte counts.
//!
//! Used for the `/info` command and status output: compact, two-unit
//! formatting instead of raw seconds or bytes.

use std::fmt;
use std::time::Duration;

/// Render a [`Duration`] as a compact human string.
///
/// Shows the two most significant non-zero units; trailing units are
/// truncated. Sub-second precision is dropped, so durations below one second
/// render as `"0s"`.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use candyriya_utils::format_uptime;
///
/// assert_eq!(format_uptime(Duration::ZERO), "0s");
/// assert_eq!(format_uptime(Duration::from_secs(45)), "45s");
/// assert_eq!(format_uptime(Duration::from_secs(90)), "1m 30s");
/// assert_eq!(format_uptime(Duration::from_secs(4320)), "1h 12m");
/// assert_eq!(format_uptime(Duration::from_secs(3600)), "1h");
/// assert_eq!(format_uptime(Duration::from_secs(86400)), "1d");
/// assert_eq!(format_uptime(Duration::from_secs(266400)), "3d 2h");
/// ```
#[must_use]
pub fn format_uptime(duration: Duration) -> String {
    const UNITS: [(u64, &str); 4] = [(86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")];

    let mut remaining = duration.as_secs();
    if remaining == 0 {
        return "0s".to_owned();
    }

    let mut parts = Vec::with_capacity(2);
    for (unit_secs, suffix) in UNITS {
        let value = remaining / unit_secs;
        remaining %= unit_secs;
        if value > 0 {
            parts.push(format!("{value}{suffix}"));
            if parts.len() == 2 {
                break;
            }
        }
    }
    parts.join(" ")
}

/// Display wrapper around [`Duration`] that renders via [`format_uptime`].
///
/// ```
/// use std::time::Duration;
/// use candyriya_utils::PrettyDuration;
///
/// assert_eq!(PrettyDuration(Duration::from_secs(90)).to_string(), "1m 30s");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrettyDuration(pub Duration);

impl fmt::Display for PrettyDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_uptime(self.0))
    }
}

impl From<Duration> for PrettyDuration {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

/// Render a byte count as a human string using IEC units (1024-based).
///
/// Values below 1 KiB are shown as exact bytes (`"512 B"`). Larger values show
/// a single fraction digit, with a trailing `.0` trimmed (`"1 KiB"`, `"2.4
/// KiB"`, `"1 MiB"`). Because of rounding, a value just below a unit boundary
/// may render as e.g. `"1024 KiB"`.
///
/// # Examples
///
/// ```
/// use candyriya_utils::human_bytes;
///
/// assert_eq!(human_bytes(0), "0 B");
/// assert_eq!(human_bytes(512), "512 B");
/// assert_eq!(human_bytes(1023), "1023 B");
/// assert_eq!(human_bytes(1024), "1 KiB");
/// assert_eq!(human_bytes(2457), "2.4 KiB");
/// assert_eq!(human_bytes(1024 * 1024), "1 MiB");
/// ```
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 7] = [
        ("B", 1),
        ("KiB", 1024),
        ("MiB", 1024 * 1024),
        ("GiB", 1024 * 1024 * 1024),
        ("TiB", 1024 * 1024 * 1024 * 1024),
        ("PiB", 1024 * 1024 * 1024 * 1024 * 1024),
        ("EiB", 1024 * 1024 * 1024 * 1024 * 1024 * 1024),
    ];

    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let (unit, unit_size) = UNITS
        .iter()
        .rev()
        .find(|(_, size)| bytes >= *size)
        .copied()
        .expect("units cover all of u64");
    let value = bytes as f64 / unit_size as f64;
    let rounded = format!("{value:.1}");
    let trimmed = rounded.strip_suffix(".0").unwrap_or(&rounded);
    format!("{trimmed} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_zero() {
        assert_eq!(format_uptime(Duration::ZERO), "0s");
    }

    #[test]
    fn uptime_sub_second_truncates_to_zero() {
        assert_eq!(format_uptime(Duration::from_millis(999)), "0s");
    }

    #[test]
    fn uptime_seconds() {
        assert_eq!(format_uptime(Duration::from_secs(1)), "1s");
        assert_eq!(format_uptime(Duration::from_secs(45)), "45s");
        assert_eq!(format_uptime(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn uptime_minutes() {
        assert_eq!(format_uptime(Duration::from_secs(60)), "1m");
        assert_eq!(format_uptime(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_uptime(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn uptime_hours() {
        assert_eq!(format_uptime(Duration::from_secs(3600)), "1h");
        assert_eq!(format_uptime(Duration::from_secs(4320)), "1h 12m");
        assert_eq!(format_uptime(Duration::from_secs(86399)), "23h 59m");
    }

    #[test]
    fn uptime_days() {
        assert_eq!(format_uptime(Duration::from_secs(86400)), "1d");
        assert_eq!(format_uptime(Duration::from_secs(266400)), "3d 2h");
        assert_eq!(
            format_uptime(Duration::from_secs(7 * 86400 + 3 * 3600 + 30 * 60)),
            "7d 3h"
        );
    }

    #[test]
    fn pretty_duration_display() {
        assert_eq!(PrettyDuration::from(Duration::ZERO).to_string(), "0s");
        assert_eq!(PrettyDuration(Duration::from_secs(3600)).to_string(), "1h");
    }

    #[test]
    fn human_bytes_bytes_range() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1), "1 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
    }

    #[test]
    fn human_bytes_kib_range() {
        assert_eq!(human_bytes(1024), "1 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(2457), "2.4 KiB");
        assert_eq!(human_bytes(2048), "2 KiB");
    }

    #[test]
    fn human_bytes_mib_range() {
        assert_eq!(human_bytes(1024 * 1024), "1 MiB");
        assert_eq!(human_bytes(1024 * 1024 + 512 * 1024), "1.5 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3 MiB");
    }

    #[test]
    fn human_bytes_larger_units() {
        assert_eq!(human_bytes(1024u64.pow(3)), "1 GiB");
        assert_eq!(human_bytes(1024u64.pow(4)), "1 TiB");
        assert_eq!(human_bytes(1024u64.pow(5)), "1 PiB");
        assert_eq!(human_bytes(1024u64.pow(6)), "1 EiB");
        assert_eq!(human_bytes(u64::MAX), "16 EiB");
    }
}
