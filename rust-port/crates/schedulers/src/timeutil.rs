//! UTC civil-time math used by next-run computation.
//!
//! All timestamps are unix seconds (i64) and all calendar arithmetic runs in
//! UTC, so tests can use fixed clocks. (The Python scheduler anchors wall-clock
//! schedules in the configured IANA zone; timezone selection is a platform
//! concern deferred to the sink/CLI layer of this port.)

/// Broken-down UTC time; `dow` uses the cron convention 0=Sunday..6=Saturday.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcParts {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 = Sunday … 6 = Saturday (croniter's default numbering).
    pub dow: u32,
    pub secs_of_day: i64,
    /// Days since the 1970-01-01 epoch.
    pub days: i64,
}

/// Howard Shanks' `civil_from_days` (public domain, "How many days are there
/// in a month?" algorithms): days-since-epoch -> (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Howard Shanks' `days_from_civil`: (year, month, day) -> days-since-epoch.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

pub fn days_to_ts(days: i64, secs_of_day: i64) -> i64 {
    days * 86_400 + secs_of_day
}

/// Split a unix timestamp into UTC civil parts.
pub fn utc_parts(ts: i64) -> UtcParts {
    let days = ts.div_euclid(86_400);
    let secs_of_day = ts.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday; with 0=Sunday that is dow 4.
    let dow = ((days + 4).rem_euclid(7)) as u32;
    UtcParts {
        year,
        month,
        day,
        hour: (secs_of_day / 3600) as u32,
        minute: ((secs_of_day % 3600) / 60) as u32,
        second: (secs_of_day % 60) as u32,
        dow,
        secs_of_day,
        days,
    }
}

/// Inverse of [`utc_parts`] from explicit civil fields.
pub fn civil_to_ts(days: i64, hour: u32, minute: u32, second: u32) -> i64 {
    days_to_ts(days, hour as i64 * 3600 + minute as i64 * 60 + second as i64)
}

/// Days-since-epoch for an explicit civil date (UTC).
pub fn civil_days(year: i64, month: u32, day: u32) -> i64 {
    days_from_civil(year, month, day)
}

/// Current unix time in seconds (the ticker's clock).
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_known_instants() {
        // 2026-01-05 (Mon) 10:00:00 UTC == 1767607200
        let ts = civil_to_ts(civil_days(2026, 1, 5), 10, 0, 0);
        assert_eq!(ts, 1_767_607_200);
        let p = utc_parts(ts);
        assert_eq!((p.year, p.month, p.day), (2026, 1, 5));
        assert_eq!(p.dow, 1, "2026-01-05 is a Monday");
        // Epoch itself is Thursday (dow 4).
        assert_eq!(utc_parts(0).dow, 4);
        assert_eq!(utc_parts(86_400 * 19_723).dow, 1);
    }

    #[test]
    fn leap_day_roundtrip() {
        let ts = civil_to_ts(civil_days(2028, 2, 29), 23, 59, 0);
        let p = utc_parts(ts);
        assert_eq!((p.year, p.month, p.day, p.hour, p.minute), (2028, 2, 29, 23, 59));
    }
}
