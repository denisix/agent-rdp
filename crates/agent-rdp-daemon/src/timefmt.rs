//! Minimal UTC timestamp formatting, so diagnostics file names and
//! transcript lines are readable without pulling in a date/time crate.

use std::time::{SystemTime, UNIX_EPOCH};

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn parts(t: SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, (sod / 3600) as u32, ((sod % 3600) / 60) as u32, (sod % 60) as u32)
}

/// `YYYYMMDD-HHMMSS` in UTC - sorts chronologically, safe in file names.
pub fn utc_compact(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s) = parts(t);
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", y, mo, d, h, mi, s)
}

/// `YYYY-MM-DDTHH:MM:SSZ` in UTC.
pub fn utc_rfc3339(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s) = parts(t);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn epoch() {
        assert_eq!(utc_rfc3339(at(0)), "1970-01-01T00:00:00Z");
        assert_eq!(utc_compact(at(0)), "19700101-000000");
    }

    #[test]
    fn known_instant() {
        // 2026-09-02T14:45:01Z
        assert_eq!(utc_rfc3339(at(1_788_360_301)), "2026-09-02T14:45:01Z");
        assert_eq!(utc_compact(at(1_788_360_301)), "20260902-144501");
    }

    #[test]
    fn leap_day() {
        // 2024-02-29T23:59:59Z
        assert_eq!(utc_rfc3339(at(1_709_251_199)), "2024-02-29T23:59:59Z");
    }
}
