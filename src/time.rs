use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Formats Unix milliseconds as an RFC 3339 UTC timestamp, second precision.
pub fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", sod / 3600, (sod % 3600) / 60, sod % 60)
}

/// Coarse relative age, e.g. "5m ago", "3h ago", "2d ago"; a date past 30 days.
pub fn ago(now_ms: i64, ts_ms: i64) -> String {
    let secs = (now_ms - ts_ms).max(0) / 1000;
    match secs {
        0..=59 => "just now".into(),
        60..=3_599 => format!("{}m ago", secs / 60),
        3_600..=86_399 => format!("{}h ago", secs / 3_600),
        86_400..=2_591_999 => format!("{}d ago", secs / 86_400),
        _ => iso(ts_ms)[..10].to_string(),
    }
}

// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::{ago, iso};

    #[test]
    fn formats_relative_ages() {
        let now = 1_791_979_445_123;
        assert_eq!(ago(now, now + 5_000), "just now");
        assert_eq!(ago(now, now - 59_000), "just now");
        assert_eq!(ago(now, now - 5 * 60_000), "5m ago");
        assert_eq!(ago(now, now - 3 * 3_600_000), "3h ago");
        assert_eq!(ago(now, now - 2 * 86_400_000), "2d ago");
        assert_eq!(ago(now, now - 40 * 86_400_000), "2026-09-04");
    }

    #[test]
    fn formats_known_instants() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(iso(1_791_979_445_123), "2026-10-14T12:04:05Z");
        assert_eq!(iso(-1000), "1969-12-31T23:59:59Z");
    }
}
