use crate::models::RhythmStatus;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDate, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use std::{fs, path::{Path, PathBuf}};
use uuid::Uuid;

const SLEEP_START_HOUR: u32 = 23;
const SLEEP_WINDOW_MINUTES: u8 = 60;
const SLEEP_DURATION_MIN_MINUTES: i64 = 7 * 60 + 45;
const SLEEP_DURATION_RANGE_MINUTES: u8 = 31; // 7h45m .. 8h15m

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedRhythmPlan {
    sleep_date: String,
    sleep_at: String,
    wake_at: String,
}

fn path(base: &Path) -> PathBuf { base.join("rhythm.json") }

fn build_plan(date: NaiveDate) -> Result<PersistedRhythmPlan> {
    let random = Uuid::new_v4();
    let bytes = random.as_bytes();
    let minute = (bytes[0] % SLEEP_WINDOW_MINUTES) as u32;
    let duration_minutes = SLEEP_DURATION_MIN_MINUTES
        + i64::from(bytes[1] % SLEEP_DURATION_RANGE_MINUTES);

    let naive = date
        .and_hms_opt(SLEEP_START_HOUR, minute, 0)
        .context("build local sleep time")?;
    let sleep_at = Local
        .from_local_datetime(&naive)
        .single()
        .or_else(|| Local.from_local_datetime(&naive).earliest())
        .context("resolve local sleep time")?;
    let wake_at = sleep_at + ChronoDuration::minutes(duration_minutes);

    Ok(PersistedRhythmPlan {
        sleep_date: date.format("%Y-%m-%d").to_string(),
        sleep_at: sleep_at.to_rfc3339(),
        wake_at: wake_at.to_rfc3339(),
    })
}

fn load(base: &Path) -> Result<Option<PersistedRhythmPlan>> {
    let p = path(base);
    if !p.exists() { return Ok(None); }
    let raw = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    match serde_json::from_str::<PersistedRhythmPlan>(&raw) {
        Ok(v) => Ok(Some(v)),
        Err(_) => Ok(None),
    }
}

fn save(base: &Path, plan: &PersistedRhythmPlan) -> Result<()> {
    fs::create_dir_all(base)?;
    let target = path(base);
    let tmp = base.join(format!("rhythm.{}.tmp", Uuid::new_v4()));
    fs::write(&tmp, serde_json::to_vec_pretty(plan)?)?;
    fs::rename(&tmp, &target).with_context(|| format!("replace {}", target.display()))?;
    Ok(())
}

fn parsed(plan: &PersistedRhythmPlan) -> Option<(NaiveDate, DateTime<Local>, DateTime<Local>)> {
    let date = NaiveDate::parse_from_str(&plan.sleep_date, "%Y-%m-%d").ok()?;
    let sleep = DateTime::parse_from_rfc3339(&plan.sleep_at).ok()?.with_timezone(&Local);
    let wake = DateTime::parse_from_rfc3339(&plan.wake_at).ok()?.with_timezone(&Local);
    Some((date, sleep, wake))
}

fn ensure_plan(base: &Path) -> Result<PersistedRhythmPlan> {
    let now = Local::now();
    let today = now.date_naive();

    if let Some(existing) = load(base)? {
        if let Some((sleep_date, _sleep_at, wake_at)) = parsed(&existing) {
            // Keep today's upcoming plan, and also keep yesterday's plan while
            // its sleep period is still active after midnight.
            if sleep_date == today || now < wake_at {
                return Ok(existing);
            }
        }
    }

    // If the companion is first launched in the early morning, reconstruct a
    // plausible previous-night plan. If its randomized wake time is already in
    // the past, immediately roll forward to today's evening plan.
    let initial_date = if now.hour() < 8 {
        today - ChronoDuration::days(1)
    } else {
        today
    };
    let mut plan = build_plan(initial_date)?;
    if let Some((_date, _sleep_at, wake_at)) = parsed(&plan) {
        if now >= wake_at && initial_date != today {
            plan = build_plan(today)?;
        }
    }
    save(base, &plan)?;
    Ok(plan)
}

pub fn status(base: &Path) -> Result<RhythmStatus> {
    let plan = ensure_plan(base)?;
    let (_date, sleep_at, wake_at) = parsed(&plan).context("parse rhythm plan")?;
    let now = Local::now();
    let sleeping = now >= sleep_at && now < wake_at;

    Ok(RhythmStatus {
        sleeping,
        sleep_at_local: sleep_at.to_rfc3339(),
        wake_at_local: wake_at.to_rfc3339(),
        sleep_at_display: sleep_at.format("%H:%M").to_string(),
        wake_at_display: wake_at.format("%H:%M").to_string(),
    })
}
