//! Human-readable formatting and size parsing.

/// Format a byte count, e.g. `1.5 GB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// Format a count with thousands separators.
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Parse a size like `10`, `500K`, `2.5M`, `1G`, `4GB`.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let digits_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != ',')
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(digits_end);
    let num: f64 = num.replace(',', "").parse().ok()?;

    let mult: f64 = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let v = num * mult;
    if v < 0.0 || !v.is_finite() {
        return None;
    }
    Some(v as u64)
}

/// Format a Unix timestamp as `YYYY-MM-DD HH:MM`, or `-` when unknown.
///
/// Implemented directly rather than pulling in a date crate: the index stores
/// plain Unix seconds and this is the only place it needs rendering.
pub fn timestamp(unix: u32) -> String {
    if unix == 0 {
        return "-".into();
    }
    let secs = unix as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi) = (tod / 3600, (tod % 3600) / 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}

/// Days since the Unix epoch to a calendar date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Current Unix time in seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format an elapsed number of seconds as a short human phrase.
pub fn duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_byte_counts() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1024), "1.00 KB");
        assert_eq!(bytes(1536), "1.50 KB");
        assert_eq!(bytes(1024 * 1024), "1.00 MB");
        assert_eq!(bytes(150 * 1024 * 1024), "150 MB");
        assert_eq!(bytes(15 * 1024 * 1024), "15.0 MB");
        // The measured D: total, as a sanity check on the human form.
        assert_eq!(bytes(668_800_000_000), "623 GB");
    }

    #[test]
    fn formats_counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(1_516_374), "1,516,374");
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("10"), Some(10));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1kb"), Some(1024));
        assert_eq!(parse_size("2.5M"), Some(2_621_440));
        assert_eq!(parse_size("1G"), Some(1_073_741_824));
        assert_eq!(parse_size(" 4 GB "), Some(4 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1,000"), Some(1000));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("10X"), None);
    }

    #[test]
    fn formats_timestamps() {
        assert_eq!(timestamp(0), "-");
        assert_eq!(timestamp(1_000_000_000), "2001-09-09 01:46");
        assert_eq!(timestamp(1_787_097_600), "2026-08-19 00:00");
        assert_eq!(timestamp(1_788_134_400), "2026-08-31 00:00");
        // Leap day, the case a hand-rolled calendar is most likely to get wrong.
        assert_eq!(timestamp(1_709_164_800), "2024-02-29 00:00");
    }
}
