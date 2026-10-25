//! Schedule model + next-run computation (port of `cron/jobs.py`:
//! `parse_schedule`, `compute_next_run`, `_compute_grace_seconds`,
//! `_classify_dispatch_lateness`).
//!
//! Timestamps are unix seconds in UTC (see `timeutil`). Supported schedule
//! forms mirror the Python tool contract:
//!   - interval:   "30m", "every 30m", "every 2h"        (recurring)
//!   - one-shot:   "in 30m", ISO "2026-02-03T14:00:00"   (fires once)
//!   - daily-time: "every day at 9am", "daily at 9:30am" (recurring)
//!   - weekly-day: "every monday 9am", "weekdays at 9am" (recurring)
//!   - cron:       "0 9 * * *" five-field expression      (recurring)

use crate::timeutil::{civil_days, civil_to_ts, days_to_ts, utc_parts};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// One stored schedule. Serialized with a `kind` tag like the Python dicts
/// `{"kind": "interval", "minutes": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Fire once at this unix timestamp.
    Once { run_at: i64 },
    /// Every N minutes (Python: `{"kind": "interval", "minutes": N}`).
    Interval { minutes: u64 },
    /// Every day at a fixed wall-clock time.
    DailyAt { hour: u32, minute: u32 },
    /// Weekly on the given days (cron dow numbers, 0=Sunday) at a fixed time.
    WeeklyAt { days: Vec<u32>, hour: u32, minute: u32 },
    /// Five-field cron expression: minute hour day-of-month month day-of-week.
    Cron { expr: String },
}

const DURATION_MINUTES: u64 = 1;
const DURATION_HOURS: u64 = 60;
const DURATION_DAYS: u64 = 1440;

/// Parse a duration into minutes: "30m" → 30, "2h" → 120, "1d" → 1440,
/// bare "hour" → 60. Port of `parse_duration`.
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim().to_lowercase();
    let (digits, unit) = match s.find(|c: char | c.is_ascii_alphabetic()) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s.as_str(), s.as_str()),
    };
    let value: u64 = if digits.is_empty() {
        1
    } else {
        digits.trim().parse().map_err(|_| anyhow!(
            "Invalid duration: '{s}'. Use format like '30m', '2h', '1d', or a bare unit like 'hour'."
        ))?
    };
    let minutes = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => value * DURATION_MINUTES,
        "h" | "hr" | "hrs" | "hour" | "hours" => value * DURATION_HOURS,
        "d" | "day" | "days" => value * DURATION_DAYS,
        _ => {
            return Err(anyhow!(
                "Invalid duration: '{s}'. Use format like '30m', '2h', '1d', or a bare unit like 'hour' (defaults to 1)."
            ))
        }
    };
    if minutes == 0 {
        return Err(anyhow!("Invalid duration: '{s}' (must be positive)."));
    }
    Ok(minutes)
}

/// Parse `9am`/`9:30am`/`14:00`/`7`/`noon`/`midnight` into a 24-hour
/// `(hour, minute)` pair. Port of `_parse_clock_time`.
pub fn parse_clock_time(text: &str) -> Result<(u32, u32)> {
    let t: String = text.trim().to_lowercase().replace(' ', "");
    if t == "noon" || t == "midday" {
        return Ok((12, 0));
    }
    if t == "midnight" {
        return Ok((0, 0));
    }
    // (\d{1,2})(?::(\d{2}))?(am|pm)?
    let byte_idx = t
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(t.len());
    let (hour_s, rest) = t.split_at(byte_idx);
    if hour_s.is_empty() || hour_s.len() > 2 {
        return Err(anyhow!("unrecognized time: {text:?}"));
    }
    let mut hour: i64 = hour_s.parse().map_err(|_| anyhow!("unrecognized time: {text:?}"))?;
    let mut minute: u32 = 0;
    let mut meridiem = "";
    let mut rest = rest.to_string();
    if rest.starts_with(':') {
        let mm: String = rest[1..].chars().take(2).collect();
        if mm.len() != 2 || !mm.chars().all(|c| c.is_ascii_digit()) {
            return Err(anyhow!("unrecognized time: {text:?}"));
        }
        minute = mm.parse().map_err(|_| anyhow!("unrecognized time: {text:?}"))?;
        rest = rest[3..].to_string();
    }
    if !rest.is_empty() {
        meridiem = &rest;
    }
    if meridiem == "am" || meridiem == "pm" {
        if !(1..=12).contains(&hour) {
            return Err(anyhow!("unrecognized 12-hour time: {text:?}"));
        }
        // Python: hour % 12 + (12 if pm else 0) — 12am → 0, 12pm → 12.
        hour = hour % 12 + if meridiem == "pm" { 12 } else { 0 };
    } else if !meridiem.is_empty() {
        return Err(anyhow!("unrecognized time: {text:?}"));
    }
    if !(0..=23).contains(&hour) || minute > 59 {
        return Err(anyhow!("time out of range: {text:?}"));
    }
    Ok((hour as u32, minute))
}

// Day-spec phrases for "every monday 9am" / "weekdays at 9am"; cron dow 0=Sun..6=Sat.
fn weekday_token(tok: &str) -> Option<u32> {
    Some(match tok {
        "sunday" | "sun" => 0,
        "monday" | "mon" => 1,
        "tuesday" | "tue" | "tues" => 2,
        "wednesday" | "wed" | "weds" => 3,
        "thursday" | "thu" | "thur" | "thurs" => 4,
        "friday" | "fri" => 5,
        "saturday" | "sat" => 6,
        _ => return None,
    })
}

/// Keyword day-specs expanding to a dow list (empty = every day).
fn keyword_days(tok: &str) -> Option<Vec<u32>> {
    match tok {
        "day" | "daily" | "everyday" => Some(vec![]),
        "weekday" | "weekdays" => Some(vec![1, 2, 3, 4, 5]),
        "weekend" | "weekends" => Some(vec![0, 6]),
        _ => None,
    }
}

/// Convert `<when> [at] <time>` ("monday 9am", "weekday at 9am",
/// "monday, wednesday at 9am") to a schedule, or None so `parse_schedule` can
/// fall back to the interval path. Port of `_natural_every_to_cron`.
fn natural_every_to_schedule(rest: &str) -> Option<Schedule> {
    let tokens: Vec<String> = rest
        .to_lowercase()
        .replace(',', " ")
        .split_whitespace()
        .map(String::from)
        .collect();
    if tokens.is_empty() {
        return None;
    }
    // Leading day tokens: a keyword spec ("weekdays") or a comma/"and"-separated weekday list.
    let (days, idx) = if let Some(d) = keyword_days(&tokens[0]) {
        (d, 1usize)
    } else {
        let mut days: Vec<u32> = Vec::new();
        let mut idx = tokens.len();
        for (i_tok, tok) in tokens.iter().enumerate() {
            if tok == "and" {
                continue;
            }
            match weekday_token(tok) {
                Some(d) => {
                    if !days.contains(&d) {
                        days.push(d);
                    }
                }
                None => {
                    idx = i_tok;
                    break;
                }
            }
        }
        if days.is_empty() {
            return None;
        }
        (days, idx)
    };
    let mut time_tokens: Vec<&str> = tokens[idx..].iter().map(String::as_str).collect();
    if time_tokens.first() == Some(&"at") {
        // optional separator: "every day at 9am"
        time_tokens.remove(0);
    }
    if time_tokens.is_empty() {
        return None;
    }
    let (hour, minute) = parse_clock_time(&time_tokens.join(" ")).ok()?;
    if days.is_empty() {
        Some(Schedule::DailyAt { hour, minute })
    } else {
        Some(Schedule::WeeklyAt { days, hour, minute })
    }
}

/// Parse a schedule string into a [`Schedule`]. Port of `parse_schedule`.
/// `now` anchors relative one-shots ("in 30m").
pub fn parse_schedule(schedule: &str, now: i64) -> Result<Schedule> {
    let schedule = schedule.trim();
    let original = schedule;
    let lower = schedule.to_lowercase();

    // Natural day/time phrase → daily/weekly ("every monday 9am", or sans
    // prefix "weekdays at 9am"); any other "every X" → recurring interval.
    let (rest, is_every) = if let Some(rest) = lower.strip_prefix("every ") {
        (rest.trim(), true)
    } else {
        (lower.as_str(), false)
    };
    if let Some(sched) = natural_every_to_schedule(rest) {
        return Ok(sched);
    }
    if is_every {
        return Ok(Schedule::Interval { minutes: parse_duration(rest)? });
    }

    // Five-field cron expression (letters allowed for named months/weekdays).
    let parts: Vec<&str> = schedule.split_whitespace().collect();
    if parts.len() == 5
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '*' | '-' | ',' | '/'))
        })
    {
        let expr = Schedule::Cron { expr: original.to_string() };
        expr.validate()?;
        return Ok(expr);
    }

    // ISO timestamp (starts with a date, optionally with a time).
    if let Some(sched) = parse_iso_once(schedule)? {
        return Ok(sched);
    }

    // "in 30m"/"in 2h" is the explicit one-shot-by-duration form; a bare
    // duration ("30m") is a RECURRING interval per the documented tool contract.
    if let Some(rest) = lower.strip_prefix("in ") {
        let minutes = parse_duration(rest.trim()).map_err(|_| {
            anyhow!("Invalid duration '{rest}' after 'in '. Use e.g. 'in 30m', 'in 2h'.")
        })?;
        return Ok(Schedule::Once { run_at: now + (minutes * 60) as i64 });
    }
    if let Ok(minutes) = parse_duration(&lower) {
        return Ok(Schedule::Interval { minutes });
    }

    Err(anyhow!(
        "Invalid schedule '{original}'. Use: interval '30m'/'every 2h'; one-shot 'in 30m' or \
         '2026-02-03T14:00:00'; weekly/daily 'every monday 9am'/'weekdays at 9am'; \
         cron '0 9 * * *'."
    ))
}

/// Minimal ISO-8601 subset: `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM` and with
/// `:SS` seconds. Naive timestamps are UTC in this port. Returns Ok(None) when
/// the string does not look like a date at all.
fn parse_iso_once(s: &str) -> Result<Option<Schedule>> {
    let bytes = s.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Ok(None);
    }
    if !s[..4].chars().all(|c| c.is_ascii_digit()) || !s[5..7].chars().all(|c| c.is_ascii_digit())
        || !s[8..10].chars().all(|c| c.is_ascii_digit())
    {
        return Ok(None);
    }
    let year: i64 = s[..4].parse().unwrap();
    let month: u32 = s[5..7].parse().unwrap();
    let day: u32 = s[8..10].parse().unwrap();
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Ok(None);
    }
    let (mut hour, mut minute, mut second) = (0u32, 0u32, 0u32);
    if bytes.len() > 10 {
        // 'T' or ' ' separator, then HH:MM[:SS]
        let time = s[11..].trim_start_matches(|c: char| c != ':' && !c.is_ascii_digit());
        let nums: Vec<&str> = time.split(':').filter(|p| !p.is_empty()).collect();
        if nums.len() < 2 {
            if nums.len() == 1 {
                return Err(anyhow!("Invalid timestamp '{s}'"));
            }
            return Ok(None);
        }
        hour = nums[0].parse().map_err(|_| anyhow!("Invalid timestamp '{s}'"))?;
        minute = nums[1].parse().map_err(|_| anyhow!("Invalid timestamp '{s}'"))?;
        if nums.len() > 2 {
            second = nums[2].parse().map_err(|_| anyhow!("Invalid timestamp '{s}'"))?;
        }
        if hour > 23 || minute > 59 || second > 59 {
            return Err(anyhow!("Invalid timestamp '{s}'"));
        }
    }
    Ok(Some(Schedule::Once {
        run_at: civil_to_ts(civil_days(year, month, day), hour, minute, second),
    }))
}

impl Schedule {
    fn validate(&self) -> Result<()> {
        if let Schedule::Cron { expr } = self {
            CronFields::parse(expr).map_err(|e| anyhow!("Invalid cron expression '{expr}': {e}"))?;
        }
        Ok(())
    }

    /// Rough cadence of one period, in seconds; None when unknown (one-shots).
    /// Used by the catch-up grace computation.
    pub fn cadence_seconds(&self) -> Option<i64> {
        match self {
            Schedule::Once { .. } => None,
            Schedule::Interval { minutes } => Some((*minutes as i64) * 60),
            Schedule::DailyAt { .. } => Some(86_400),
            Schedule::WeeklyAt { .. } => Some(7 * 86_400),
            Schedule::Cron { .. } => None, // computed on demand via next-run pairs
        }
    }
}

/// Compute the next run time for a schedule as a unix timestamp, or None if no
/// more runs. `last_run_at` anchors recurring kinds so a restart doesn't
/// re-anchor the schedule (port of `compute_next_run`). For one-shots, a set
/// `last_run_at` means the run already happened.
pub fn compute_next_run(
    schedule: &Schedule,
    last_run_at: Option<i64>,
    now: i64,
) -> Option<i64> {
    match schedule {
        Schedule::Once { run_at } => {
            if last_run_at.is_some() {
                None // already fired
            } else {
                Some(*run_at)
            }
        }
        Schedule::Interval { minutes } => {
            // Anchor on last_run_at, add in absolute time so an interval keeps
            // its duration across UTC-offset changes.
            let base = last_run_at.unwrap_or(now);
            Some(base + (*minutes as i64) * 60)
        }
        Schedule::DailyAt { hour, minute } => {
            next_daily_run(*hour, *minute, last_run_at.unwrap_or(now))
        }
        Schedule::WeeklyAt { days, hour, minute } => {
            next_weekly_run(days, *hour, *minute, last_run_at.unwrap_or(now))
        }
        Schedule::Cron { expr } => {
            let fields = CronFields::parse(expr).ok()?;
            next_cron_run(&fields, last_run_at.unwrap_or(now))
        }
    }
}

/// First wall-clock occurrence of `hour:minute` strictly after `base`.
fn next_daily_run(hour: u32, minute: u32, base: i64) -> Option<i64> {
    let p = utc_parts(base);
    let today = civil_to_ts(p.days, hour, minute, 0);
    Some(if today > base { today } else { today + 86_400 })
}

/// First occurrence on one of `days` (cron dow) at `hour:minute` strictly
/// after `base`; scans up to 7 days ahead.
fn next_weekly_run(days: &[u32], hour: u32, minute: u32, base: i64) -> Option<i64> {
    if days.is_empty() {
        return next_daily_run(hour, minute, base);
    }
    let base_days = utc_parts(base).days;
    for offset in 0..=7 {
        // dow of (base_days + offset): epoch day 0 was Thursday (dow 4).
        let dow = (base_days + 4 + offset).rem_euclid(7) as u32;
        if days.contains(&dow) {
            let candidate = civil_to_ts(base_days + offset, hour, minute, 0);
            if candidate > base {
                return Some(candidate);
            }
        }
    }
    // A week always contains every requested dow.
    Some(civil_to_ts(base_days + 8, hour, minute, 0))
}

// --- Missed-run catch-up policy (port of _compute_grace_seconds + classify) ---

const MIN_GRACE_SECONDS: i64 = 120;
const MAX_GRACE_SECONDS: i64 = 7200;
const LATE_DISPATCH_TOLERANCE_SECONDS: i64 = 300;

/// How late a job can be and still catch up rather than fast-forward: half the
/// period, clamped to [120s, 2h], so daily jobs catch up but frequent jobs
/// fast-forward quickly.
pub fn grace_seconds(schedule: &Schedule) -> i64 {
    match schedule.cadence_seconds() {
        None => {
            // Estimate cadence for cron jobs from two successive computed runs.
            let first = compute_next_run(schedule, None, 0);
            match first {
                Some(f) => match compute_next_run(schedule, Some(f), f) {
                    Some(s) => ((s - f) / 2).clamp(MIN_GRACE_SECONDS, MAX_GRACE_SECONDS),
                    None => MIN_GRACE_SECONDS,
                },
                None => MIN_GRACE_SECONDS,
            }
        }
        Some(period) => {
            if period <= 0 {
                MIN_GRACE_SECONDS
            } else {
                (period / 2).clamp(MIN_GRACE_SECONDS, MAX_GRACE_SECONDS)
            }
        }
    }
}

/// How a dispatch compares to its scheduled time: `OnTime` (within ticker
/// slack), `Late` (within the catch-up grace window), or `CatchUp` (beyond
/// grace; accumulated misses skipped, executed once now).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lateness {
    OnTime,
    Late,
    CatchUp,
}

pub fn classify_dispatch_lateness(lateness_seconds: i64, grace_seconds: i64) -> Lateness {
    if lateness_seconds > grace_seconds {
        Lateness::CatchUp
    } else if lateness_seconds > LATE_DISPATCH_TOLERANCE_SECONDS {
        Lateness::Late
    } else {
        Lateness::OnTime
    }
}

// --- Minimal cron expression engine (croniter subset) ---

#[derive(Debug, Clone, PartialEq, Eq)]
struct CronFields {
    minutes: Vec<u32>, // sorted set
    hours: Vec<u32>,
    doms: Vec<u32>,
    months: Vec<u32>,
    dows: Vec<u32>,
    dom_star: bool,
    dow_star: bool,
}

fn field_set(field: &str, low: u32, high: u32) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| format!("bad step {s:?}"))?),
            None => (part, 1),
        };
        if step == 0 {
            return Err("step cannot be 0".into());
        }
        let (start, end) = if range == "*" {
            (low, high)
        } else if let Some((a, b)) = range.split_once('-') {
            (
                a.parse::<u32>().map_err(|_| format!("bad number {a:?}"))?,
                b.parse::<u32>().map_err(|_| format!("bad number {b:?}"))?,
            )
        } else if range.chars().all(|c| c.is_ascii_digit()) {
            let v: u32 = range.parse().map_err(|_| format!("bad number {range:?}"))?;
            if v < low || v > high {
                return Err(format!("number {v} out of [{low},{high}]"));
            }
            if step != 1 {
                (v, high) // "N/step" means "N-high/step" in cron
            } else {
                out.push(v);
                continue;
            }
        } else {
            return Err(format!("unsupported field token {range:?} (named days/months not supported)"));
        };
        if start < low || end > high || start > end {
            return Err(format!("range {range:?} out of [{low},{high}]"));
        }
        let mut v = start;
        while v <= end {
            out.push(v);
            v += step;
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

impl CronFields {
    fn parse(expr: &str) -> Result<CronFields, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!("expected 5 fields, got {}", fields.len()));
        }
        let minutes = field_set(fields[0], 0, 59)?;
        let hours = field_set(fields[1], 0, 23)?;
        let doms = field_set(fields[2], 1, 31)?;
        let months = field_set(fields[3], 1, 12)?;
        let mut dows = field_set(fields[4], 0, 6)?;
        // croniter's default allows 7 as Sunday; normalize it to 0.
        for d in dows.iter_mut() {
            if *d == 7 {
                *d = 0;
            }
        }
        dows.sort_unstable();
        dows.dedup();
        Ok(CronFields {
            dom_star: fields[2] == "*",
            dow_star: fields[4] == "*",
            minutes,
            hours,
            doms,
            months,
            dows,
        })
    }

    fn day_matches(&self, month: u32, dow: u32, dom: u32) -> bool {
        let dom_ok = self.dom_star || self.doms.contains(&dom);
        let dow_ok = self.dow_star || self.dows.contains(&dow);
        // Standard cron: when both are restricted, EITHER matches.
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (true, false) => dow_ok,
            (false, true) => dom_ok,
            (false, false) => dom_ok || dow_ok,
            }
        .then_some(())
        .is_some()
            && self.months.contains(&month)
    }
}

/// First matching instant strictly after `base`, scanning day-by-day (cron
/// expressions can never repeat within one day).
fn next_cron_run(f: &CronFields, base: i64) -> Option<i64> {
    let base_days = utc_parts(base).days;
    let floor_minute = utc_parts(base).secs_of_day / 60 + 1;
    for offset in 0..(366 * 4 + 2) {
        let day = base_days + offset;
        let p = utc_parts(day * 86_400);
        if !f.day_matches(p.month, p.dow, p.day) {
            continue;
        }
        // Strictly-after guard for today: never return a run in the past.
        let floor = if offset == 0 { floor_minute } else { 0 };
        for &h in &f.hours {
            for &m in &f.minutes {
                let mins = (h * 60 + m) as i64;
                if mins >= floor {
                    return Some(days_to_ts(day, mins * 60));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeutil::civil_to_ts;

    /// Fixed clock: Monday 2026-01-05 10:00:00 UTC.
    const NOW: i64 = 1_767_607_200;

    fn at(days_from_now: i64, hour: u32, minute: u32) -> i64 {
        civil_to_ts(civil_days(2026, 1, 5) + days_from_now, hour, minute, 0)
    }

    #[test]
    fn parses_intervals() {
        for (text, minutes) in [
            ("30m", 30),
            ("every 30m", 30),
            ("every 2h", 120),
            ("2h", 120),
            ("1d", 1440),
            ("every 90m", 90),
            ("hour", 60),
            ("every 45 min", 45),
            ("3 days", 4320),
        ] {
            assert_eq!(
                parse_schedule(text, NOW).unwrap(),
                Schedule::Interval { minutes },
                "input {text}"
            );
        }
        assert!(parse_duration("12x").is_err());
        assert!(parse_schedule("every ", NOW).is_err());
    }

    #[test]
    fn parses_one_shots() {
        match parse_schedule("in 30m", NOW).unwrap() {
            Schedule::Once { run_at } => assert_eq!(run_at, NOW + 1800),
            other => panic!("{other:?}"),
        }
        match parse_schedule("in 2h", NOW).unwrap() {
            Schedule::Once { run_at } => assert_eq!(run_at, NOW + 7200),
            other => panic!("{other:?}"),
        }
        match parse_schedule("2026-02-03T14:00:00", NOW).unwrap() {
            Schedule::Once { run_at } => assert_eq!(run_at, at(29, 14, 0)),
            other => panic!("{other:?}"),
        }
        match parse_schedule("2026-02-03", NOW).unwrap() {
            Schedule::Once { run_at } => assert_eq!(run_at, at(29, 0, 0)),
            other => panic!("{other:?}"),
        }
        assert!(parse_schedule("2026-13-40", NOW).is_err());
        assert!(parse_schedule("garbage", NOW).is_err());
    }

    #[test]
    fn parses_daily_and_weekly() {
        assert_eq!(
            parse_schedule("every day at 9am", NOW).unwrap(),
            Schedule::DailyAt { hour: 9, minute: 0 }
        );
        assert_eq!(
            parse_schedule("daily at 9:30am", NOW).unwrap(),
            Schedule::DailyAt { hour: 9, minute: 30 }
        );
        assert_eq!(
            parse_schedule("every monday 9am", NOW).unwrap(),
            Schedule::WeeklyAt { days: vec![1], hour: 9, minute: 0 }
        );
        assert_eq!(
            parse_schedule("every monday, wednesday at noon", NOW).unwrap(),
            Schedule::WeeklyAt { days: vec![1, 3], hour: 12, minute: 0 }
        );
        assert_eq!(
            parse_schedule("weekdays at 9am", NOW).unwrap(),
            Schedule::WeeklyAt { days: vec![1, 2, 3, 4, 5], hour: 9, minute: 0 }
        );
        assert_eq!(
            parse_schedule("every weekends 8:15pm", NOW).unwrap(),
            Schedule::WeeklyAt { days: vec![0, 6], hour: 20, minute: 15 }
        );
        // Bare "9am" is not a day spec → falls through to the interval path
        // and errors, matching the Python.
        assert!(parse_schedule("9am", NOW).is_err());
    }

    #[test]
    fn parses_cron_expressions() {
        assert_eq!(
            parse_schedule("0 9 * * *", NOW).unwrap(),
            Schedule::Cron { expr: "0 9 * * *".into() }
        );
        assert_eq!(
            parse_schedule("*/15 8-18 * * 1-5", NOW).unwrap(),
            Schedule::Cron { expr: "*/15 8-18 * * 1-5".into() }
        );
        assert!(parse_schedule("0 9 * *", NOW).is_err()); // 4 fields
        assert!(parse_schedule("61 9 * * *", NOW).is_err()); // minute out of range
    }

    #[test]
    fn next_run_interval_anchors_on_last_run() {
        let s = Schedule::Interval { minutes: 30 };
        // Restart with no last_run: anchored on now.
        assert_eq!(compute_next_run(&s, None, NOW), Some(NOW + 1800));
        // Anchored on last_run so a restart doesn't re-anchor the schedule.
        assert_eq!(compute_next_run(&s, Some(NOW - 600), NOW), Some(NOW + 1200));
        // After a run at NOW, next is cadence later.
        assert_eq!(compute_next_run(&s, Some(NOW), NOW), Some(NOW + 1800));
    }

    #[test]
    fn next_run_once_semantics() {
        let future = Schedule::Once { run_at: NOW + 100 };
        assert_eq!(compute_next_run(&future, None, NOW), Some(NOW + 100));
        assert_eq!(compute_next_run(&future, Some(NOW), NOW), None, "already fired");
        let past = Schedule::Once { run_at: NOW - 100 };
        // Past one-shot is still due (the tick fires it immediately).
        assert_eq!(compute_next_run(&past, None, NOW), Some(NOW - 100));
    }

    #[test]
    fn next_run_daily() {
        let s = Schedule::DailyAt { hour: 9, minute: 0 };
        // Monday 10:00 → next occurrence is Tuesday 09:00.
        assert_eq!(compute_next_run(&s, None, NOW), Some(at(1, 9, 0)));
        // Monday 08:30 → today 09:00.
        assert_eq!(
            compute_next_run(&s, None, at(0, 8, 30)),
            Some(at(0, 9, 0))
        );
        // Exactly at 09:00 → strictly after → tomorrow.
        assert_eq!(
            compute_next_run(&s, None, at(0, 9, 0)),
            Some(at(1, 9, 0))
        );
    }

    #[test]
    fn next_run_weekly() {
        let mon = Schedule::WeeklyAt { days: vec![1], hour: 9, minute: 0 };
        // Monday 10:00 → next Monday 09:00.
        assert_eq!(compute_next_run(&mon, None, NOW), Some(at(7, 9, 0)));
        let monwed = Schedule::WeeklyAt { days: vec![1, 3], hour: 9, minute: 0 };
        // Monday 10:00 → Wednesday 09:00.
        assert_eq!(compute_next_run(&monwed, None, NOW), Some(at(2, 9, 0)));
        // Wednesday noon → weekend days 0/6: Saturday (Jan 10) comes first.
        let weekend = Schedule::WeeklyAt { days: vec![0, 6], hour: 8, minute: 0 };
        assert_eq!(
            compute_next_run(&weekend, None, at(2, 12, 0)),
            Some(at(5, 8, 0))
        );
    }

    #[test]
    fn next_run_cron() {
        let s = Schedule::Cron { expr: "30 9 * * *".into() };
        assert_eq!(compute_next_run(&s, None, NOW), Some(at(1, 9, 30)));
        let every30 = Schedule::Cron { expr: "*/30 * * * *".into() };
        // 10:00 exactly → strictly after → 10:30.
        assert_eq!(compute_next_run(&every30, None, NOW), Some(at(0, 10, 30)));
        assert_eq!(
            compute_next_run(&every30, None, at(0, 10, 30)),
            Some(at(0, 11, 0))
        );
        // Dow-only expression: next Sunday noon after Monday 10:00 is Jan 11.
        let sundays = Schedule::Cron { expr: "0 12 * * 0".into() };
        assert_eq!(compute_next_run(&sundays, None, NOW), Some(at(6, 12, 0)));
        // dom OR dow when both restricted: Mondays OR the 1st; Monday Jan 12 wins.
        let mixed = Schedule::Cron { expr: "0 6 1 * 1".into() };
        assert_eq!(compute_next_run(&mixed, None, NOW), Some(at(7, 6, 0)));
    }

    #[test]
    fn grace_and_lateness_match_python() {
        // Half the period clamped to [120, 7200].
        assert_eq!(grace_seconds(&Schedule::Interval { minutes: 1 }), 120);
        assert_eq!(grace_seconds(&Schedule::Interval { minutes: 60 }), 1800);
        assert_eq!(grace_seconds(&Schedule::Interval { minutes: 30 }), 900);
        assert_eq!(grace_seconds(&Schedule::DailyAt { hour: 9, minute: 0 }), 7200);
        assert_eq!(grace_seconds(&Schedule::Once { run_at: NOW }), 120);
        let cron = Schedule::Cron { expr: "0 9 * * *".into() };
        assert_eq!(grace_seconds(&cron), 7200);

        let g = 1800;
        assert_eq!(classify_dispatch_lateness(0, g), Lateness::OnTime);
        assert_eq!(classify_dispatch_lateness(299, g), Lateness::OnTime);
        assert_eq!(classify_dispatch_lateness(300, g), Lateness::OnTime, "strict > 300");
        assert_eq!(classify_dispatch_lateness(301, g), Lateness::Late);
        assert_eq!(classify_dispatch_lateness(1800, g), Lateness::Late);
        assert_eq!(classify_dispatch_lateness(1801, g), Lateness::CatchUp);
    }

    #[test]
    fn schedule_serde_roundtrip() {
        let scheds = [
            Schedule::Once { run_at: 42 },
            Schedule::Interval { minutes: 30 },
            Schedule::DailyAt { hour: 9, minute: 5 },
            Schedule::WeeklyAt { days: vec![1, 3], hour: 7, minute: 15 },
            Schedule::Cron { expr: "0 9 * * *".into() },
        ];
        for s in scheds {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<Schedule>(&json).unwrap(), s);
        }
    }
}
