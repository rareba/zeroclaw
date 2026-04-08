use crate::cron::{CronJob, Schedule};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cron::Schedule as CronExprSchedule;
use std::str::FromStr;

/// Default deadline window in seconds (1 hour).
const DEFAULT_DEADLINE_WINDOW_SECS: u64 = 3600;

pub fn next_run_for_schedule(schedule: &Schedule, from: DateTime<Utc>) -> Result<DateTime<Utc>> {
    match schedule {
        Schedule::Cron { expr, tz } => {
            let normalized = normalize_expression(expr)?;
            let cron = CronExprSchedule::from_str(&normalized)
                .with_context(|| format!("Invalid cron expression: {expr}"))?;

            if let Some(tz_name) = tz {
                let timezone = chrono_tz::Tz::from_str(tz_name)
                    .with_context(|| format!("Invalid IANA timezone: {tz_name}"))?;
                let localized_from = from.with_timezone(&timezone);
                let next_local = cron.after(&localized_from).next().ok_or_else(|| {
                    anyhow::anyhow!("No future occurrence for expression: {expr}")
                })?;
                Ok(next_local.with_timezone(&Utc))
            } else {
                // Default to OS local timezone so schedules match user
                // expectations instead of always using UTC (#5220).
                let local_from = from.with_timezone(&chrono::Local);
                let next_local = cron.after(&local_from).next().ok_or_else(|| {
                    anyhow::anyhow!("No future occurrence for expression: {expr}")
                })?;
                Ok(next_local.with_timezone(&Utc))
            }
        }
        Schedule::At { at } => Ok(*at),
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            let ms = i64::try_from(*every_ms).context("every_ms is too large")?;
            let delta = ChronoDuration::milliseconds(ms);
            from.checked_add_signed(delta)
                .ok_or_else(|| anyhow::anyhow!("every_ms overflowed DateTime"))
        }
    }
}

/// Compute the next run time for a `CronJob`, taking deadline acceleration
/// into account.
///
/// When a job has `deadline_at` and `deadline_interval_ms` configured:
/// - If `now >= deadline_at`, the deadline has passed — use normal schedule.
/// - If the normal next_run would land inside the deadline window (i.e.
///   `now + normal_interval >= deadline_at - deadline_window`), switch to the
///   faster `deadline_interval_ms`.
/// - Otherwise, use the normal schedule.
pub fn next_run_for_job(job: &CronJob, from: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let normal_next = next_run_for_schedule(&job.schedule, from)?;

    let Some(deadline_at) = job.deadline_at else {
        return Ok(normal_next);
    };
    let Some(deadline_interval_ms) = job.deadline_interval_ms else {
        return Ok(normal_next);
    };

    // Deadline already passed — revert to normal schedule.
    if from >= deadline_at {
        tracing::info!(
            job_id = %job.id,
            deadline = %deadline_at,
            "Deadline passed, reverting to normal schedule"
        );
        return Ok(normal_next);
    }

    let window_secs = job
        .deadline_window_secs
        .unwrap_or(DEFAULT_DEADLINE_WINDOW_SECS);
    let window = ChronoDuration::seconds(i64::try_from(window_secs).unwrap_or(i64::MAX));
    let window_start = deadline_at - window;

    // Check if we are inside the deadline window.
    if from >= window_start || normal_next >= window_start {
        let ms = i64::try_from(deadline_interval_ms).context("deadline_interval_ms too large")?;
        let fast_next = from
            .checked_add_signed(ChronoDuration::milliseconds(ms))
            .ok_or_else(|| anyhow::anyhow!("deadline_interval_ms overflowed DateTime"))?;

        // Don't schedule past the deadline itself.
        let clamped = if fast_next > deadline_at {
            deadline_at
        } else {
            fast_next
        };

        tracing::info!(
            job_id = %job.id,
            deadline = %deadline_at,
            normal_next = %normal_next,
            fast_next = %clamped,
            deadline_interval_ms,
            "Deadline approaching, switching to faster interval"
        );
        return Ok(clamped);
    }

    Ok(normal_next)
}

pub fn validate_schedule(schedule: &Schedule, now: DateTime<Utc>) -> Result<()> {
    match schedule {
        Schedule::Cron { expr, .. } => {
            let _ = normalize_expression(expr)?;
            let _ = next_run_for_schedule(schedule, now)?;
            Ok(())
        }
        Schedule::At { at } => {
            if *at <= now {
                anyhow::bail!("Invalid schedule: 'at' must be in the future");
            }
            Ok(())
        }
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            Ok(())
        }
    }
}

pub fn schedule_cron_expression(schedule: &Schedule) -> Option<String> {
    match schedule {
        Schedule::Cron { expr, .. } => Some(expr.clone()),
        _ => None,
    }
}

pub fn normalize_expression(expression: &str) -> Result<String> {
    let expression = expression.trim();
    let field_count = expression.split_whitespace().count();

    match field_count {
        // standard crontab syntax: minute hour day month weekday
        // Normalize weekday field from standard crontab semantics (0/7=Sun, 1=Mon, …, 6=Sat)
        // to cron-crate semantics (1=Sun, 2=Mon, …, 7=Sat).
        5 => {
            let mut fields: Vec<&str> = expression.split_whitespace().collect();
            let weekday = fields[4];
            let normalized_weekday = normalize_weekday_field(weekday)?;
            fields[4] = &normalized_weekday;
            Ok(format!(
                "0 {} {} {} {} {}",
                fields[0], fields[1], fields[2], fields[3], fields[4]
            ))
        }
        // crate-native syntax includes seconds (+ optional year)
        6 | 7 => Ok(expression.to_string()),
        _ => anyhow::bail!(
            "Invalid cron expression: {expression} (expected 5, 6, or 7 fields, got {field_count})"
        ),
    }
}

/// Translate a single numeric weekday value from standard crontab semantics
/// (0 or 7 = Sunday, 1 = Monday, …, 6 = Saturday) to cron-crate semantics
/// (1 = Sunday, 2 = Monday, …, 7 = Saturday).
fn translate_weekday_value(val: u8) -> Result<u8> {
    match val {
        0 | 7 => Ok(1), // Sunday
        1..=6 => Ok(val + 1),
        _ => anyhow::bail!("Invalid weekday value: {val} (expected 0-7)"),
    }
}

/// Normalize the weekday field of a 5-field cron expression from standard
/// crontab numbering to cron-crate numbering. Passes through `*`, named days
/// (e.g. `MON`, `MON-FRI`), and already-valid tokens unchanged.
fn normalize_weekday_field(field: &str) -> Result<String> {
    // Asterisk and wildcard variants pass through unchanged.
    if field == "*" || field == "?" {
        return Ok(field.to_string());
    }

    // If the field contains any alphabetic character it uses named days
    // (e.g. MON-FRI) which the cron crate handles natively.
    if field.chars().any(|c| c.is_ascii_alphabetic()) {
        return Ok(field.to_string());
    }

    // The field may be a comma-separated list of items, where each item is
    // either a single value, a range (start-end), or a range/value with a
    // step (/N).
    let parts: Vec<&str> = field.split(',').collect();
    let mut result_parts = Vec::with_capacity(parts.len());

    for part in parts {
        // Split off optional step suffix first (e.g. "1-5/2" → "1-5" + "2").
        let (range_part, step) = if let Some((r, s)) = part.split_once('/') {
            (r, Some(s))
        } else {
            (part, None)
        };

        let translated = if let Some((start_s, end_s)) = range_part.split_once('-') {
            let start: u8 = start_s
                .parse()
                .with_context(|| format!("Invalid weekday in range: {start_s}"))?;
            let end: u8 = end_s
                .parse()
                .with_context(|| format!("Invalid weekday in range: {end_s}"))?;
            let new_start = translate_weekday_value(start)?;
            let new_end = translate_weekday_value(end)?;
            format!("{new_start}-{new_end}")
        } else if range_part == "*" {
            "*".to_string()
        } else {
            let val: u8 = range_part
                .parse()
                .with_context(|| format!("Invalid weekday value: {range_part}"))?;
            translate_weekday_value(val)?.to_string()
        };

        if let Some(s) = step {
            result_parts.push(format!("{translated}/{s}"));
        } else {
            result_parts.push(translated);
        }
    }

    Ok(result_parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Offset, TimeZone};

    #[test]
    fn next_run_for_schedule_supports_every_and_at() {
        let now = Utc::now();
        let every = Schedule::Every { every_ms: 60_000 };
        let next = next_run_for_schedule(&every, now).unwrap();
        assert!(next > now);

        let at = now + ChronoDuration::minutes(10);
        let at_schedule = Schedule::At { at };
        let next_at = next_run_for_schedule(&at_schedule, now).unwrap();
        assert_eq!(next_at, at);
    }

    #[test]
    fn next_run_for_schedule_supports_timezone() {
        let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: Some("America/Los_Angeles".into()),
        };

        let next = next_run_for_schedule(&schedule, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 16, 17, 0, 0).unwrap());
    }

    #[test]
    fn normalize_weekday_field_translates_standard_crontab_values() {
        // Single values: standard crontab → cron crate
        assert_eq!(normalize_weekday_field("0").unwrap(), "1"); // Sun
        assert_eq!(normalize_weekday_field("1").unwrap(), "2"); // Mon
        assert_eq!(normalize_weekday_field("5").unwrap(), "6"); // Fri
        assert_eq!(normalize_weekday_field("6").unwrap(), "7"); // Sat
        assert_eq!(normalize_weekday_field("7").unwrap(), "1"); // Sun (alias)
    }

    #[test]
    fn normalize_weekday_field_translates_ranges() {
        // 1-5 (Mon-Fri) → 2-6
        assert_eq!(normalize_weekday_field("1-5").unwrap(), "2-6");
        // 0-6 (Sun-Sat) → 1-7
        assert_eq!(normalize_weekday_field("0-6").unwrap(), "1-7");
    }

    #[test]
    fn normalize_weekday_field_translates_lists() {
        // 0,6 (Sun,Sat) → 1,7
        assert_eq!(normalize_weekday_field("0,6").unwrap(), "1,7");
        // 1,3,5 (Mon,Wed,Fri) → 2,4,6
        assert_eq!(normalize_weekday_field("1,3,5").unwrap(), "2,4,6");
    }

    #[test]
    fn normalize_weekday_field_translates_steps() {
        // 1-5/2 (Mon-Fri every other) → 2-6/2
        assert_eq!(normalize_weekday_field("1-5/2").unwrap(), "2-6/2");
        // */2 (every other day) → */2
        assert_eq!(normalize_weekday_field("*/2").unwrap(), "*/2");
    }

    #[test]
    fn normalize_weekday_field_passes_through_wildcards_and_names() {
        assert_eq!(normalize_weekday_field("*").unwrap(), "*");
        assert_eq!(normalize_weekday_field("?").unwrap(), "?");
        assert_eq!(normalize_weekday_field("MON-FRI").unwrap(), "MON-FRI");
        assert_eq!(
            normalize_weekday_field("MON,WED,FRI").unwrap(),
            "MON,WED,FRI"
        );
    }

    #[test]
    fn normalize_expression_applies_weekday_fix_to_5_field() {
        // "0 9 * * 1-5" should become "0 0 9 * * 2-6"
        let result = normalize_expression("0 9 * * 1-5").unwrap();
        assert_eq!(result, "0 0 9 * * 2-6");
    }

    #[test]
    fn normalize_expression_does_not_modify_6_field() {
        // 6-field expressions already use cron-crate semantics
        let result = normalize_expression("0 0 9 * * 1-5").unwrap();
        assert_eq!(result, "0 0 9 * * 1-5");
    }

    #[test]
    fn weekday_1_5_schedules_monday_through_friday() {
        // 2026-02-16 is a Monday. With "0 9 * * 1-5" (Mon-Fri at 09:00 UTC),
        // the next run from Sunday 2026-02-15 should be Monday 2026-02-16.
        let sunday = Utc.with_ymd_and_hms(2026, 2, 15, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * 1-5".into(),
            tz: Some("UTC".into()),
        };
        let next = next_run_for_schedule(&schedule, sunday).unwrap();
        // Should be Monday 2026-02-16 at 09:00 UTC (weekday = Mon)
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 16, 9, 0, 0).unwrap());
        assert_eq!(next.weekday(), chrono::Weekday::Mon);
    }

    #[test]
    fn weekday_1_5_does_not_fire_on_saturday_or_sunday() {
        // From Friday evening, next run should skip Sat/Sun → Monday
        let friday_evening = Utc.with_ymd_and_hms(2026, 2, 20, 18, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * 1-5".into(),
            tz: Some("UTC".into()),
        };
        let next = next_run_for_schedule(&schedule, friday_evening).unwrap();
        // Should be Monday 2026-02-23 at 09:00 UTC
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 23, 9, 0, 0).unwrap());
        assert_eq!(next.weekday(), chrono::Weekday::Mon);
    }

    #[test]
    fn weekday_0_means_sunday() {
        // "0 10 * * 0" should fire on Sunday only
        let monday = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 10 * * 0".into(),
            tz: Some("UTC".into()),
        };
        let next = next_run_for_schedule(&schedule, monday).unwrap();
        assert_eq!(next.weekday(), chrono::Weekday::Sun);
    }

    #[test]
    fn weekday_7_means_sunday() {
        // "0 10 * * 7" should also fire on Sunday (alias)
        let monday = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 10 * * 7".into(),
            tz: Some("UTC".into()),
        };
        let next = next_run_for_schedule(&schedule, monday).unwrap();
        assert_eq!(next.weekday(), chrono::Weekday::Sun);
    }

    #[test]
    fn no_tz_defaults_to_local_timezone() {
        let from = Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        let schedule_no_tz = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: None,
        };
        let schedule_utc = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: Some("UTC".into()),
        };
        let next_local = next_run_for_schedule(&schedule_no_tz, from).unwrap();
        let next_utc = next_run_for_schedule(&schedule_utc, from).unwrap();
        assert!(next_local > from);
        assert!(next_utc > from);
        let local_offset = chrono::Local::now().offset().fix().local_minus_utc();
        if local_offset == 0 {
            assert_eq!(next_local, next_utc);
        } else {
            assert_ne!(next_local, next_utc);
        }
    }

    // ── Deadline-aware scheduling tests ──────────────────────────────

    use crate::cron::types::{DeliveryConfig, JobType, SessionTarget};

    /// Helper to build a minimal CronJob for deadline tests.
    fn make_test_job(schedule: Schedule) -> CronJob {
        CronJob {
            id: "test-deadline".to_string(),
            expression: String::new(),
            schedule,
            command: "echo test".to_string(),
            prompt: None,
            name: None,
            job_type: JobType::Shell,
            session_target: SessionTarget::Isolated,
            model: None,
            enabled: true,
            delivery: DeliveryConfig::default(),
            delete_after_run: false,
            deadline_at: None,
            deadline_interval_ms: None,
            deadline_window_secs: None,
            allowed_tools: None,
            source: "imperative".to_string(),
            created_at: Utc::now(),
            next_run: Utc::now(),
            last_run: None,
            last_status: None,
            last_output: None,
        }
    }

    #[test]
    fn next_run_for_job_without_deadline_returns_normal() {
        let now = Utc.with_ymd_and_hms(2026, 3, 30, 10, 0, 0).unwrap();
        let job = make_test_job(Schedule::Every { every_ms: 3_600_000 });
        let next = next_run_for_job(&job, now).unwrap();
        // Normal: 1 hour later
        assert_eq!(next, now + ChronoDuration::hours(1));
    }

    #[test]
    fn next_run_for_job_inside_deadline_window_uses_fast_interval() {
        let now = Utc.with_ymd_and_hms(2026, 3, 30, 10, 0, 0).unwrap();
        let deadline = Utc.with_ymd_and_hms(2026, 3, 30, 10, 30, 0).unwrap();

        let mut job = make_test_job(Schedule::Every { every_ms: 3_600_000 }); // 1h normal
        job.deadline_at = Some(deadline);
        job.deadline_interval_ms = Some(60_000); // 1 minute fast
        job.deadline_window_secs = Some(3600); // 1 hour window

        let next = next_run_for_job(&job, now).unwrap();
        // Should use 1-minute interval, not 1-hour
        assert_eq!(next, now + ChronoDuration::minutes(1));
    }

    #[test]
    fn next_run_for_job_outside_deadline_window_uses_normal() {
        let now = Utc.with_ymd_and_hms(2026, 3, 30, 8, 0, 0).unwrap();
        let deadline = Utc.with_ymd_and_hms(2026, 3, 30, 12, 0, 0).unwrap();

        let mut job = make_test_job(Schedule::Every { every_ms: 60_000 }); // 1 min normal
        job.deadline_at = Some(deadline);
        job.deadline_interval_ms = Some(10_000); // 10s fast
        job.deadline_window_secs = Some(3600); // 1 hour window => starts at 11:00

        let next = next_run_for_job(&job, now).unwrap();
        // Normal next: 08:01 — well before 11:00 window start, and normal_next (08:01) < 11:00
        // So should use normal schedule
        assert_eq!(next, now + ChronoDuration::minutes(1));
    }

    #[test]
    fn next_run_for_job_deadline_passed_uses_normal() {
        let now = Utc.with_ymd_and_hms(2026, 3, 30, 13, 0, 0).unwrap();
        let deadline = Utc.with_ymd_and_hms(2026, 3, 30, 12, 0, 0).unwrap();

        let mut job = make_test_job(Schedule::Every { every_ms: 3_600_000 });
        job.deadline_at = Some(deadline);
        job.deadline_interval_ms = Some(60_000);
        job.deadline_window_secs = Some(3600);

        let next = next_run_for_job(&job, now).unwrap();
        // Deadline passed, reverts to normal 1-hour interval
        assert_eq!(next, now + ChronoDuration::hours(1));
    }

    #[test]
    fn next_run_for_job_fast_interval_clamped_to_deadline() {
        let now = Utc.with_ymd_and_hms(2026, 3, 30, 11, 59, 50).unwrap();
        let deadline = Utc.with_ymd_and_hms(2026, 3, 30, 12, 0, 0).unwrap();

        let mut job = make_test_job(Schedule::Every { every_ms: 3_600_000 });
        job.deadline_at = Some(deadline);
        job.deadline_interval_ms = Some(60_000); // 1 minute, would overshoot
        job.deadline_window_secs = Some(3600);

        let next = next_run_for_job(&job, now).unwrap();
        // 1 minute from now = 12:00:50, which is past deadline 12:00:00
        // So should be clamped to deadline
        assert_eq!(next, deadline);
    }

    #[test]
    fn next_run_for_job_uses_default_window_when_not_specified() {
        let now = Utc.with_ymd_and_hms(2026, 3, 30, 11, 30, 0).unwrap();
        let deadline = Utc.with_ymd_and_hms(2026, 3, 30, 12, 0, 0).unwrap();

        let mut job = make_test_job(Schedule::Every { every_ms: 7_200_000 }); // 2h normal
        job.deadline_at = Some(deadline);
        job.deadline_interval_ms = Some(60_000);
        // No deadline_window_secs => defaults to 3600 (1h)
        // window_start = 12:00 - 1h = 11:00, now (11:30) >= 11:00 => in window

        let next = next_run_for_job(&job, now).unwrap();
        assert_eq!(next, now + ChronoDuration::minutes(1));
    }

    #[test]
    fn next_run_for_job_no_deadline_interval_ms_returns_normal() {
        let now = Utc.with_ymd_and_hms(2026, 3, 30, 11, 30, 0).unwrap();
        let deadline = Utc.with_ymd_and_hms(2026, 3, 30, 12, 0, 0).unwrap();

        let mut job = make_test_job(Schedule::Every { every_ms: 3_600_000 });
        job.deadline_at = Some(deadline);
        // No deadline_interval_ms => no fast interval, use normal

        let next = next_run_for_job(&job, now).unwrap();
        assert_eq!(next, now + ChronoDuration::hours(1));
    }
}
