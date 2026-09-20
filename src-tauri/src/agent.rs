use crate::{config, db, llm, logging, models::{ActivitySnapshot, DiaryEntry}, rhythm, screenpipe, AppState};
use anyhow::{anyhow, Context, Result};
use chrono::{Duration as ChronoDuration, Local, Timelike, Utc};
use std::{sync::{Arc, atomic::Ordering}, time::{SystemTime, UNIX_EPOCH}};
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;
use tokio::time::{sleep, Duration, Instant};

const SCHEDULER_TICK_SECONDS: u64 = 15;
const DIARY_RETRY_GAP_SECONDS: u64 = 5 * 60;

fn next_observation_delay(cfg: &config::AppConfig) -> Duration {
    let min = cfg.observation_interval_min_seconds.max(60);
    let max = cfg.observation_interval_max_seconds.max(min);
    if min == max {
        return Duration::from_secs(min);
    }
    let entropy = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(min);
    Duration::from_secs(min + entropy % (max - min + 1))
}

pub async fn run_loop(app: AppHandle, state: Arc<AppState>) {
    let mut next_observation = Instant::now();
    let mut was_sleeping = false;
    let mut last_diary_attempt: Option<(String, Instant)> = None;

    loop {
        if state.shutting_down.load(Ordering::SeqCst) {
            logging::info("background loop stopped for application shutdown");
            break;
        }

        let cfg = match config::load(&state.data_dir) {
            Ok(v) => v,
            Err(e) => {
                logging::error(&format!("config: {e:#}"));
                sleep(Duration::from_secs(60)).await;
                continue;
            }
        };

        let rhythm_status = match rhythm::status(&state.data_dir) {
            Ok(v) => v,
            Err(e) => {
                logging::error(&format!("rhythm: {e:#}"));
                sleep(Duration::from_secs(SCHEDULER_TICK_SECONDS)).await;
                continue;
            }
        };

        if rhythm_status.sleeping {
            if !was_sleeping {
                let _ = app.emit("rhythm_changed", &rhythm_status);
            }
            was_sleeping = true;
            // Sleeping means no screenpipe queries and no LLM API calls.
            sleep(Duration::from_secs(SCHEDULER_TICK_SECONDS)).await;
            continue;
        }

        if was_sleeping {
            was_sleeping = false;
            next_observation = Instant::now();
            let _ = app.emit("rhythm_changed", &rhythm_status);
        }

        // Diary timing is checked independently from the observation cadence so
        // the 22:30 write happens within roughly one scheduler tick.
        let date = Local::now().format("%Y-%m-%d").to_string();
        let diary_retry_ready = last_diary_attempt
            .as_ref()
            .map(|(d, at)| d != &date || at.elapsed() >= Duration::from_secs(DIARY_RETRY_GAP_SECONDS))
            .unwrap_or(true);
        if diary_retry_ready {
            let local = Local::now();
            let reached = (local.hour(), local.minute()) >= (cfg.diary_hour, cfg.diary_minute);
            if reached {
                last_diary_attempt = Some((date.clone(), Instant::now()));
                if let Err(e) = maybe_make_diary(&app, &state, &cfg).await {
                    logging::error(&format!("diary: {e:#}"));
                }
            }
        }

        if Instant::now() >= next_observation {
            if let Err(e) = observe_once(&app, &state, &cfg).await {
                logging::error(&format!("observe: {e:#}"));
            }
            let delay = next_observation_delay(&cfg);
            logging::info(&format!("next automatic observation in {}s", delay.as_secs()));
            next_observation = Instant::now() + delay;
        }

        sleep(Duration::from_secs(SCHEDULER_TICK_SECONDS)).await;
    }
}

pub async fn observe_once(app: &AppHandle, state: &Arc<AppState>, cfg: &config::AppConfig) -> Result<()> {
    if rhythm::status(&state.data_dir)?.sleeping { return Ok(()); }

    // Manual observation can coincide with the periodic loop. Serialize our own
    // searches so we do not create avoidable screenpipe 503 backpressure.
    let _observation_guard = state.observation_lock.lock().await;

    // Start the local recorder automatically when the companion is awake. This
    // also makes login-start useful without requiring a separate terminal.
    screenpipe::ensure_daemon(&state.http, &cfg.screenpipe_url, &state.data_dir).await?;

    if cfg.gemini_api_key.trim().is_empty() { return Ok(()); }
    let end = Utc::now();
    // Cover at least the maximum automatic interval so a 5-10 minute cadence
    // cannot leave unseen gaps between snapshots.
    let cadence_window = ((cfg.observation_interval_max_seconds + 59) / 60) as i64;
    let window_minutes = cfg.observation_window_minutes.max(cadence_window).max(1);
    let start = end - ChronoDuration::minutes(window_minutes);
    let mut snapshot = screenpipe::collect(&state.http, &cfg.screenpipe_url, &cfg.screenpipe_api_key, start, end).await?;
    snapshot.phone = db::phone_summary_since(&state.db_path, &start.to_rfc3339())?;
    let recent = db::recent_observations(&state.db_path, 8)?;
    let decision = llm::observe_and_decide(&state.http, cfg, &snapshot, &recent).await?;

    // Persistent snapshot deliberately drops raw OCR/accessibility snippets.
    let persistent_snapshot = ActivitySnapshot { transient_text_snippets: vec![], ..snapshot };
    let now = Utc::now().to_rfc3339();
    db::insert_observation(&state.db_path, &now, &persistent_snapshot, &decision)?;
    let _ = app.emit("observation", &decision);

    // A long observation could straddle the randomly selected bedtime. Do not
    // start a proactive Gemini call once sleep has begun.
    if rhythm::status(&state.data_dir)?.sleeping { return Ok(()); }

    if decision.should_speak && notification_allowed(state, cfg)? && !cfg.gemini_api_key.trim().is_empty() {
        let recent_messages = db::recent_messages(&state.db_path, 14)?;
        let text = llm::render_proactive_message(&state.http, cfg, &decision, &recent_messages).await?;
        db::insert_message(&state.db_path, &Utc::now().to_rfc3339(), "assistant", &text, true)?;
        let _ = app.notification().builder().title(&cfg.companion_name).body(&text).show();
        let _ = app.emit("new_message", &text);
    }
    Ok(())
}

fn notification_allowed(state: &Arc<AppState>, cfg: &config::AppConfig) -> Result<bool> {
    let now = Utc::now();
    if let Some(last) = db::last_proactive_time(&state.db_path)? {
        if let Ok(last) = chrono::DateTime::parse_from_rfc3339(&last) {
            if now.signed_duration_since(last.with_timezone(&Utc)).num_minutes() < cfg.minimum_notification_gap_minutes { return Ok(false); }
        }
    }
    let hour_ago = (now - ChronoDuration::hours(1)).to_rfc3339();
    Ok(db::notification_count_since(&state.db_path, &hour_ago)? < cfg.max_notifications_per_hour)
}

pub async fn maybe_make_diary(app: &AppHandle, state: &Arc<AppState>, cfg: &config::AppConfig) -> Result<Option<DiaryEntry>> {
    if rhythm::status(&state.data_dir)?.sleeping { return Ok(None); }
    if cfg.gemini_api_key.trim().is_empty() { return Ok(None); }

    let local = Local::now();
    let reached = (local.hour(), local.minute()) >= (cfg.diary_hour, cfg.diary_minute);
    if !reached { return Ok(None); }
    let date = local.format("%Y-%m-%d").to_string();
    if db::get_diary(&state.db_path, &date)?.is_some() { return Ok(None); }
    let d = generate_diary_for_today(state, cfg).await?;
    let _ = app.emit("diary_ready", &d);
    Ok(Some(d))
}

pub async fn generate_diary_for_today(state: &Arc<AppState>, cfg: &config::AppConfig) -> Result<DiaryEntry> {
    if rhythm::status(&state.data_dir)?.sleeping {
        return Err(anyhow!("{}は寝ています。起きてから日記を生成してください。", cfg.companion_name));
    }

    let local = Local::now();
    let date = local.format("%Y-%m-%d").to_string();
    let start_local = local.date_naive().and_hms_opt(0,0,0).unwrap().and_local_timezone(Local).single().context("local midnight")?;
    let since = start_local.with_timezone(&Utc).to_rfc3339();
    let observations = db::observations_since(&state.db_path, &since)?;
    let messages = db::recent_messages(&state.db_path, 80)?;
    let text = llm::diary(&state.http, cfg, &date, &observations, &messages).await?;
    let created_at = Utc::now().to_rfc3339();
    db::save_diary(&state.db_path, &date, &created_at, &text)?;
    Ok(DiaryEntry { date, created_at, text })
}
