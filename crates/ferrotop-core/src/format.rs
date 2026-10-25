//! Display formatting that matches System Informer's conventions.

/// Byte count with binary units: `"512 B"`, `"1.23 kB"`, `"12.3 MB"`, `"123 GB"`. Zero is blank.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    if n == 0 {
        return String::new();
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{n} B"),
        _ if value >= 100.0 => format!("{value:.0} {}", UNITS[unit]),
        _ if value >= 10.0 => format!("{value:.1} {}", UNITS[unit]),
        _ => format!("{value:.2} {}", UNITS[unit]),
    }
}

/// Transfer rate, blank when idle: `"1.20 MB/s"`.
pub fn rate(bytes_per_second: f64) -> String {
    if bytes_per_second < 1.0 {
        return String::new();
    }
    format!("{}/s", bytes(bytes_per_second as u64))
}

/// CPU share (0.0..=1.0 of the whole machine) as a percentage with two decimals, the way
/// System Informer shows it: blank when idle and `"< 0.01"` for tiny but non-zero usage.
pub fn cpu(fraction: f32) -> String {
    let percent = fraction * 100.0;
    if percent <= 0.0 {
        String::new()
    } else if percent < 0.01 {
        "< 0.01".into()
    } else {
        format!("{percent:.2}")
    }
}

/// Network throughput with decimal units, like Task Manager: `"136 Kbps"`, `"2.5 Mbps"`.
pub fn bits_per_second(bits: f64) -> String {
    const UNITS: [&str; 4] = ["Kbps", "Mbps", "Gbps", "Tbps"];
    let mut value = bits.max(0.0) / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value < 10.0 && value != 0.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Up time as `days:hh:mm:ss`, like Task Manager.
pub fn uptime(seconds: u64) -> String {
    format!("{}:{:02}:{:02}:{:02}", seconds / 86_400, seconds / 3_600 % 24, seconds / 60 % 60, seconds % 60)
}

/// Clock speed: `"4.13 GHz"`.
pub fn mhz(mhz: f64) -> String {
    format!("{:.2} GHz", mhz / 1000.0)
}

/// Integer with thousands separators: `"160,312"`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Coarse elapsed time: `"42s"`, `"5m 12s"`, `"2h 13m"`, `"3d 4h"`.
pub fn duration(seconds: u64) -> String {
    let (d, h, m, s) = (seconds / 86_400, seconds / 3_600 % 24, seconds / 60 % 60, seconds % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m {s}s"),
        (0, _, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// `yyyymmdd-hhmmss` in UTC for a Unix time, for file names that sort by date.
pub fn timestamp(unix_seconds: u64) -> String {
    let days = unix_seconds / 86_400;
    let secs = unix_seconds % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

pub fn timestamp_now() -> String {
    timestamp(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(timestamp(0), "19700101-000000");
        assert_eq!(timestamp(951_782_400), "20000229-000000");
        assert_eq!(timestamp(1_700_000_000), "20231114-221320");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.50 kB");
        assert_eq!(bytes(12 * 1024 * 1024 + 300 * 1024), "12.3 MB");
        assert_eq!(bytes(123 * 1024 * 1024 * 1024), "123 GB");
    }

    #[test]
    fn formats_cpu_like_system_informer() {
        assert_eq!(cpu(0.0), "");
        assert_eq!(cpu(0.000_01), "< 0.01");
        assert_eq!(cpu(0.1234), "12.34");
    }

    #[test]
    fn formats_counts_and_durations() {
        assert_eq!(count(7), "7");
        assert_eq!(count(1_234), "1,234");
        assert_eq!(count(160_312), "160,312");
        assert_eq!(duration(42), "42s");
        assert_eq!(duration(312), "5m 12s");
        assert_eq!(duration(2 * 3600 + 13 * 60), "2h 13m");
        assert_eq!(duration(3 * 86_400 + 4 * 3600), "3d 4h");
    }

    #[test]
    fn formats_network_and_clock_figures() {
        assert_eq!(bits_per_second(0.0), "0 Kbps");
        assert_eq!(bits_per_second(136_000.0), "136 Kbps");
        assert_eq!(bits_per_second(2_500_000.0), "2.5 Mbps");
        assert_eq!(bits_per_second(2_500_000_000.0), "2.5 Gbps");
        assert_eq!(uptime(3 * 86_400 + 4 * 3600 + 5 * 60 + 6), "3:04:05:06");
        assert_eq!(mhz(4130.0), "4.13 GHz");
    }

    #[test]
    fn rate_is_blank_when_idle() {
        assert_eq!(rate(0.0), "");
        assert_eq!(rate(1.5 * 1024.0 * 1024.0), "1.50 MB/s");
    }
}
