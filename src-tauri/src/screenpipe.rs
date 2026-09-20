use crate::models::{ActivitySnapshot, AppActivity};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::Value;
use std::{collections::{HashMap, HashSet}, fs::OpenOptions, path::Path, process::{Command, Stdio}};
use tokio::time::{sleep, Duration};

fn authed(req: RequestBuilder, api_key: &str) -> RequestBuilder {
    let key = if api_key.trim().is_empty() {
        std::env::var("SCREENPIPE_LOCAL_API_KEY").unwrap_or_default()
    } else {
        api_key.trim().to_string()
    };
    let req = req
        .header("X-Screenpipe-Client", "api")
        .header("X-Screenpipe-Agent", "yandere-companion");
    if key.trim().is_empty() { req } else { req.bearer_auth(key.trim()) }
}

async fn json_response(req: RequestBuilder, label: &str) -> Result<Value> {
    // screenpipe deliberately rejects a cache-miss search with 503 when its
    // small route-wide search admission pool is full. That is backpressure,
    // not a broken recorder, so retry a few times instead of surfacing a
    // transient error to the companion UI.
    for attempt in 0..4u32 {
        let current = req
            .try_clone()
            .ok_or_else(|| anyhow!("{label}: request could not be cloned for retry"))?;
        let res = current
            .send()
            .await
            .with_context(|| format!("{label}: request failed"))?;
        let status = res.status();
        let retry_after = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let url = res.url().clone();
        let body = res
            .text()
            .await
            .with_context(|| format!("{label}: read response body"))?;

        if status.is_success() {
            return serde_json::from_str(&body)
                .with_context(|| format!("{label}: invalid JSON from {url}"));
        }

        let transient = status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::TOO_MANY_REQUESTS;
        if transient && attempt < 3 {
            let body_delay = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("retry_after_ms").and_then(Value::as_u64));
            let delay_ms = retry_after
                .map(|secs| secs.saturating_mul(1000))
                .or(body_delay)
                .unwrap_or_else(|| 350u64.saturating_mul(1u64 << attempt));
            sleep(Duration::from_millis(delay_ms.clamp(200, 2500))).await;
            continue;
        }

        let detail = if body.trim().is_empty() {
            "(empty response body)"
        } else {
            body.trim()
        };
        return Err(anyhow!("{label}: HTTP {status} for {url}: {detail}"));
    }

    Err(anyhow!("{label}: retries exhausted"))
}

pub async fn health(client: &Client, base: &str) -> bool {
    client.get(format!("{}/health", base.trim_end_matches('/')))
        .send().await.map(|r| r.status().is_success()).unwrap_or(false)
}

fn local_screenpipe_url(base: &str) -> bool {
    let base = base.trim().to_ascii_lowercase();
    base.starts_with("http://127.0.0.1:") || base.starts_with("http://localhost:")
}

/// Start the local screenpipe recorder when it is not already running.
/// The GUI app may start from launchd with a minimal PATH, so a login zsh is
/// used to pick up Homebrew/nvm configuration. An installed `screenpipe`
/// binary is preferred; `npx screenpipe@latest` is the fallback.
pub async fn ensure_daemon(client: &Client, base: &str, data_dir: &Path) -> Result<()> {
    if health(client, base).await { return Ok(()); }
    if !local_screenpipe_url(base) {
        return Err(anyhow!("screenpipe is not reachable at configured non-local URL: {base}"));
    }

    std::fs::create_dir_all(data_dir)?;
    let log_path = data_dir.join("screenpipe.log");
    let stdout = OpenOptions::new().create(true).append(true).open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let stderr = stdout.try_clone()?;

    let args = "record --disable-audio --app-context memory --disable-keyboard-capture=false --disable-clipboard-capture=true --capture-scroll=true --prioritize-input-latency --disable-meeting-detector --disable-telemetry --retention-days 14 --retention-mode media";
    let script = format!(
        "if command -v screenpipe >/dev/null 2>&1; then exec screenpipe {args}; else exec npx -y screenpipe@latest {args}; fi"
    );

    Command::new("/bin/zsh")
        .arg("-lc")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("start screenpipe")?;

    // `npx` may need a moment to resolve the package after login.
    for _ in 0..45 {
        sleep(Duration::from_secs(1)).await;
        if health(client, base).await { return Ok(()); }
    }
    Err(anyhow!("screenpipe did not become healthy after automatic start; see {}", log_path.display()))
}

/// Checks both daemon health and authenticated API access. `/health` itself is
/// intentionally unauthenticated in current screenpipe releases, so it cannot
/// detect a missing local API token.
#[allow(dead_code)]
pub async fn ready(client: &Client, base: &str, api_key: &str) -> bool {
    if !health(client, base).await { return false; }
    let base = base.trim_end_matches('/');
    let req = authed(client.get(format!("{base}/search")), api_key)
        .query(&[("limit", "1"), ("include_frames", "false"), ("include_cloud", "false")]);
    req.send().await.map(|r| {
        let status = r.status();
        status.is_success()
            // A busy response proves that the authenticated route is reachable;
            // do not turn the tray indicator red just because search is loaded.
            || status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::TOO_MANY_REQUESTS
    }).unwrap_or(false)
}

pub async fn collect(client: &Client, base: &str, api_key: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<ActivitySnapshot> {
    let base = base.trim_end_matches('/');
    let common = [
        ("start_time", start.to_rfc3339()),
        ("end_time", end.to_rfc3339()),
        ("limit", "500".to_string()),
        ("include_frames", "false".to_string()),
        ("include_cloud", "false".to_string()),
    ];

    let all = json_response(
        authed(client.get(format!("{base}/search")), api_key)
            .query(&common)
            .query(&[("content_type", "all"), ("max_content_length", "600")]),
        "screenpipe /search(all)",
    ).await?;

    let input = json_response(
        authed(client.get(format!("{base}/search")), api_key)
            .query(&common)
            .query(&[("content_type", "input")]),
        "screenpipe /search(input)",
    ).await.unwrap_or(Value::Null);

    let mut app_counts: HashMap<String, usize> = HashMap::new();
    let mut titles = Vec::new();
    let mut title_seen = HashSet::new();
    let mut snippets = Vec::new();
    let mut records = 0usize;

    if let Some(items) = all.get("data").and_then(Value::as_array) {
        records = items.len();
        for item in items {
            let content = item.get("content").unwrap_or(item);
            let focused = content.get("focused").and_then(Value::as_bool).unwrap_or(true);
            if !focused { continue; }
            if let Some(app) = content.get("app_name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                *app_counts.entry(app.to_string()).or_default() += 1;
            }
            if let Some(title) = content.get("window_name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                if title_seen.insert(title.to_string()) && titles.len() < 12 { titles.push(trim_to_chars(title, 120)); }
            }
            if snippets.len() < 8 {
                if let Some(text) = content.get("text").and_then(Value::as_str) {
                    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if compact.len() >= 20 { snippets.push(trim_to_chars(&compact, 360)); }
                }
            }
        }
    }

    let mut keyboard = 0usize;
    let mut mouse = 0usize;
    let mut input_total = 0usize;
    if let Some(items) = input.get("data").and_then(Value::as_array) {
        input_total = items.len();
        for item in items {
            match classify_input_metadata(item) {
                InputKind::Keyboard => keyboard += 1,
                InputKind::Mouse => mouse += 1,
                InputKind::Other => {}
            }
        }
    }

    let total_secs = (end - start).num_seconds().max(0) as u64;
    let count_sum: usize = app_counts.values().sum();
    let mut apps: Vec<AppActivity> = app_counts.into_iter().map(|(app, count)| {
        let seconds = if count_sum == 0 { 0 } else { (total_secs as f64 * count as f64 / count_sum as f64).round() as u64 };
        AppActivity { app, seconds }
    }).collect();
    apps.sort_by_key(|a| std::cmp::Reverse(a.seconds));
    apps.truncate(8);

    let activity_signal = input_total.saturating_add(records);
    let idle = if activity_signal == 0 { total_secs } else if input_total < 3 && records < 3 { total_secs.saturating_mul(2) / 3 } else { 0 };

    Ok(ActivitySnapshot {
        start_time: start.to_rfc3339(),
        end_time: end.to_rfc3339(),
        foreground_apps: apps,
        window_titles: titles,
        keyboard_events: keyboard,
        mouse_events: mouse,
        input_events_total: input_total,
        observed_records: records,
        idle_seconds_estimate: idle,
        transient_text_snippets: snippets,
    })
}

#[derive(Copy, Clone)] enum InputKind { Keyboard, Mouse, Other }

fn classify_input_metadata(v: &Value) -> InputKind {
    // Deliberately inspect only type-ish metadata. Never inspect text/key value fields.
    let content = v.get("content").unwrap_or(v);
    for key in ["event_type", "input_type", "kind", "device", "type"] {
        if let Some(s) = content.get(key).and_then(Value::as_str) {
            let s = s.to_ascii_lowercase();
            if s.contains("key") || s.contains("keyboard") { return InputKind::Keyboard; }
            if s.contains("mouse") || s.contains("click") || s.contains("scroll") || s.contains("pointer") { return InputKind::Mouse; }
        }
    }
    InputKind::Other
}

fn trim_to_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
