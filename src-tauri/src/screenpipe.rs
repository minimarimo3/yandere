use crate::{logging, models::{ActivitySnapshot, AppActivity}};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder, StatusCode, Url};
use serde_json::Value;
use std::{collections::{HashMap, HashSet}, fs::OpenOptions, path::Path, process::{Command, Stdio}};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
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
            logging::warn(&format!("{label}: HTTP {status} from {url}; retry {}/4", attempt + 1));
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
        let message = format!("{label}: HTTP {status} for {url}: {detail}");
        logging::error(&message);
        return Err(anyhow!(message));
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
    logging::info(&format!("screenpipe is not running at {base}; starting recorder"));
    if !local_screenpipe_url(base) {
        return Err(anyhow!("screenpipe is not reachable at configured non-local URL: {base}"));
    }

    std::fs::create_dir_all(data_dir)?;
    let log_path = data_dir.join("screenpipe.log");
    let stdout = OpenOptions::new().create(true).append(true).open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let stderr = stdout.try_clone()?;

    let args = "record --disable-audio --app-context memory --disable-keyboard-capture=false --disable-clipboard-capture=true --capture-scroll=true --prioritize-input-latency --disable-meeting-detector --disable-telemetry --retention-days 14 --retention-mode media --idle-capture-interval-ms 30000";
    let script = format!(
        "if command -v screenpipe >/dev/null 2>&1; then exec screenpipe {args}; else exec npx -y screenpipe@latest {args}; fi"
    );

    let mut command = Command::new("/bin/zsh");
    command
        .arg("-lc")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    // Put the complete npx/node/native screenpipe chain in its own process
    // group.  Killing only the final TCP listener leaves the npm/node parents
    // alive, which in turn can keep macOS' screen-recording indicator around.
    #[cfg(unix)]
    command.process_group(0);

    let child = command.spawn().context("start screenpipe")?;
    let launcher_pid = child.id();
    std::fs::write(data_dir.join("screenpipe.pid"), launcher_pid.to_string())
        .context("write screenpipe launcher pid")?;
    logging::info(&format!("started screenpipe process group pgid={launcher_pid}"));

    // `npx` may need a moment to resolve the package after login.
    for _ in 0..45 {
        sleep(Duration::from_secs(1)).await;
        if health(client, base).await {
            logging::info(&format!("screenpipe became healthy at {base}"));
            return Ok(());
        }
    }
    let message = format!("screenpipe did not become healthy after automatic start; see {}", log_path.display());
    logging::error(&message);
    Err(anyhow!(message))
}

fn local_port(base: &str) -> Result<u16> {
    if !local_screenpipe_url(base) {
        return Err(anyhow!("refusing to stop non-local screenpipe URL: {base}"));
    }
    let url = Url::parse(base).with_context(|| format!("parse screenpipe URL {base}"))?;
    url.port_or_known_default().ok_or_else(|| anyhow!("screenpipe URL has no port: {base}"))
}

/// Stop the local screenpipe recorder and its launcher chain.
///
/// When this app starts screenpipe through `npx`, the process tree is normally
/// `npm exec -> node -> native screenpipe`.  The native child owns port 3030,
/// but killing only that child is not enough: its npm/node parents survive.
/// New launches are therefore placed in their own Unix process group and the
/// group id is stored in `screenpipe.pid`.  Shutdown terminates that whole
/// group.  A legacy fallback also walks upward from the TCP listener so an
/// already-running recorder created by v0.1.11 or older is cleaned up too.
pub async fn stop_daemon(_client: &Client, base: &str, data_dir: &Path) -> Result<()> {
    let port = local_port(base)?;
    logging::info(&format!("stopping screenpipe on TCP port {port}"));

    let pid_path = data_dir.join("screenpipe.pid");
    if let Ok(raw) = std::fs::read_to_string(&pid_path) {
        if let Ok(pgid) = raw.trim().parse::<i32>() {
            // Validate the persisted pid before sending a signal.  A stale pid
            // may eventually be reused by an unrelated process.
            let command = Command::new("/bin/ps")
                .args(["-p", &pgid.to_string(), "-o", "command="])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            if command.to_ascii_lowercase().contains("screenpipe") {
                logging::info(&format!("terminating tracked screenpipe process group pgid={pgid}"));
                #[cfg(unix)]
                unsafe {
                    // Negative pid means process group.
                    libc::kill(-pgid, libc::SIGTERM);
                }
                for _ in 0..10 {
                    sleep(Duration::from_millis(100)).await;
                    #[cfg(unix)]
                    let alive = unsafe { libc::kill(-pgid, 0) == 0 };
                    #[cfg(not(unix))]
                    let alive = false;
                    if !alive { break; }
                }
                #[cfg(unix)]
                unsafe {
                    if libc::kill(-pgid, 0) == 0 {
                        logging::warn(&format!("screenpipe process group pgid={pgid} survived TERM; sending KILL"));
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                }
            } else if !command.trim().is_empty() {
                logging::warn(&format!("ignoring stale screenpipe.pid={pgid}; process is: {}", command.trim()));
            }
        }
    }
    let _ = std::fs::remove_file(&pid_path);

    // Legacy cleanup for recorders started by older companion versions (or
    // manually with npx). Start at each listener and walk through parent
    // processes while their command line still belongs to `screenpipe record`.
    // This catches npm exec + node + native screenpipe without killing an
    // unrelated parent shell or the companion itself.
    let script = format!(r#"
        listeners=$(/usr/sbin/lsof -tiTCP:{port} -sTCP:LISTEN 2>/dev/null | tr '\n' ' ')
        targets=""
        for listener in $listeners; do
          current="$listener"
          first=1
          while [ -n "$current" ] && [ "$current" -gt 1 ] 2>/dev/null; do
            cmd=$(/bin/ps -p "$current" -o command= 2>/dev/null || true)
            if [ "$first" = 1 ]; then
              targets="$targets $current"
              first=0
            else
              case "$cmd" in
                *screenpipe*record*) targets="$targets $current" ;;
                *) break ;;
              esac
            fi
            parent=$(/bin/ps -p "$current" -o ppid= 2>/dev/null | tr -d ' ')
            [ -z "$parent" ] && break
            current="$parent"
          done
        done

        targets=$(printf '%s\n' $targets | /usr/bin/sort -u | tr '\n' ' ')
        if [ -n "$targets" ]; then
          /bin/kill -TERM $targets 2>/dev/null || true
          for _ in 1 2 3 4 5 6 7 8 9 10; do
            sleep 0.1
            alive=""
            for pid in $targets; do
              if /bin/kill -0 "$pid" 2>/dev/null; then alive="$alive $pid"; fi
            done
            [ -z "$alive" ] && break
          done
          for pid in $targets; do
            if /bin/kill -0 "$pid" 2>/dev/null; then
              /bin/kill -KILL "$pid" 2>/dev/null || true
            fi
          done
        fi
    "#);

    let output = Command::new("/bin/zsh")
        .arg("-lc")
        .arg(script)
        .stdin(Stdio::null())
        .output()
        .context("stop legacy screenpipe process tree")?;
    if !output.status.success() {
        logging::warn(&format!("legacy screenpipe cleanup exited with {}", output.status));
    }

    // Verify the actual recorder listener is gone before reporting success.
    let remaining = Command::new("/usr/sbin/lsof")
        .args([&format!("-tiTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if remaining.is_empty() {
        logging::info("screenpipe process tree stopped");
        Ok(())
    } else {
        let message = format!("screenpipe listener still present after shutdown; pid(s): {remaining}");
        logging::error(&message);
        Err(anyhow!(message))
    }
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
