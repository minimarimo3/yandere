use crate::{config, db, logging, models::{PhoneActivityEvent, PhoneReceiverStatus}, rhythm, AppState};
use anyhow::{anyhow, Result};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde_json::json;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    process::Command,
    sync::{atomic::Ordering, Arc},
};
use tokio::{net::TcpListener, time::{sleep, Duration}};

const RETRY_SECONDS: u64 = 15;

pub async fn run_receiver(state: Arc<AppState>) {
    loop {
        if state.shutting_down.load(Ordering::SeqCst) {
            logging::info("phone receiver stopped for application shutdown");
            break;
        }

        let cfg = match config::load(&state.data_dir) {
            Ok(v) => v,
            Err(e) => {
                set_status(&state, PhoneReceiverStatus {
                    enabled: false,
                    detail: format!("設定の読み込みに失敗: {e}"),
                    ..Default::default()
                }).await;
                sleep(Duration::from_secs(RETRY_SECONDS)).await;
                continue;
            }
        };

        if !cfg.phone_receiver_enabled {
            set_status(&state, PhoneReceiverStatus {
                enabled: false,
                port: cfg.phone_receiver_port,
                detail: "Android連携は無効です。".into(),
                ..Default::default()
            }).await;
            sleep(Duration::from_secs(RETRY_SECONDS)).await;
            continue;
        }

        let Some(ip) = detect_tailscale_ipv4() else {
            set_status(&state, PhoneReceiverStatus {
                enabled: true,
                running: false,
                port: cfg.phone_receiver_port,
                detail: "TailscaleのIPv4アドレスが見つかりません。Tailscale接続後に自動で再試行します。".into(),
                ..Default::default()
            }).await;
            sleep(Duration::from_secs(RETRY_SECONDS)).await;
            continue;
        };

        let addr = SocketAddr::new(IpAddr::V4(ip), cfg.phone_receiver_port);
        let listener = match TcpListener::bind(addr).await {
            Ok(v) => v,
            Err(e) => {
                set_status(&state, PhoneReceiverStatus {
                    enabled: true,
                    running: false,
                    tailscale_ip: Some(ip.to_string()),
                    port: cfg.phone_receiver_port,
                    endpoint: format!("http://{}:{}/v1/phone/activity", ip, cfg.phone_receiver_port),
                    detail: format!("Android受信ポートを開けません: {e}"),
                    ..Default::default()
                }).await;
                logging::error(&format!("phone receiver bind {addr}: {e}"));
                sleep(Duration::from_secs(RETRY_SECONDS)).await;
                continue;
            }
        };

        let endpoint = format!("http://{}:{}/v1/phone/activity", ip, cfg.phone_receiver_port);
        set_status(&state, PhoneReceiverStatus {
            enabled: true,
            running: true,
            tailscale_ip: Some(ip.to_string()),
            port: cfg.phone_receiver_port,
            endpoint: endpoint.clone(),
            detail: "Tailscale内だけでAndroidからの状態を待っています。".into(),
            ..Default::default()
        }).await;
        logging::info(&format!("phone receiver listening on {addr}"));

        let router = Router::new()
            .route("/v1/phone/activity", post(post_activity))
            .route("/v1/phone/health", get(phone_health))
            .with_state(state.clone());

        let server = axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_signal(state.clone()));
        if let Err(e) = server.await {
            logging::error(&format!("phone receiver server: {e}"));
        }

        if state.shutting_down.load(Ordering::SeqCst) {
            break;
        }
        set_status(&state, PhoneReceiverStatus {
            enabled: true,
            running: false,
            tailscale_ip: Some(ip.to_string()),
            port: cfg.phone_receiver_port,
            endpoint,
            detail: "Android受信サーバーが停止しました。再起動を試します。".into(),
            ..Default::default()
        }).await;
        sleep(Duration::from_secs(2)).await;
    }
}

async fn shutdown_signal(state: Arc<AppState>) {
    while !state.shutting_down.load(Ordering::SeqCst) {
        sleep(Duration::from_millis(250)).await;
    }
}

async fn post_activity(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mut event): Json<PhoneActivityEvent>,
) -> impl IntoResponse {
    let cfg = match config::load(&state.data_dir) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok":false,"error":e.to_string()}))),
    };
    if !cfg.phone_receiver_enabled {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok":false,"error":"phone receiver disabled"})));
    }
    if !authorized(&headers, &cfg.phone_receiver_token) {
        logging::warn("phone receiver rejected an unauthorized request");
        return (StatusCode::UNAUTHORIZED, Json(json!({"ok":false,"error":"unauthorized"})));
    }

    // Sleeping means observation is off. Keep the endpoint responsive so the
    // Android client does not need special reconnection logic, but do not store
    // or inspect anything while the companion is asleep.
    if rhythm::status(&state.data_dir).map(|r| r.sleeping).unwrap_or(false) {
        return (StatusCode::ACCEPTED, Json(json!({"ok":true,"stored":false,"reason":"sleeping"})));
    }

    normalize_event(&mut event);
    if let Err(e) = validate_event(&event) {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok":false,"error":e.to_string()})));
    }

    let received_at = Utc::now().to_rfc3339();
    if let Err(e) = db::insert_phone_activity(&state.db_path, &received_at, &event) {
        logging::error(&format!("store phone activity: {e:#}"));
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok":false,"error":"database error"})));
    }

    {
        let mut status = state.phone_status.write().await;
        status.last_seen_at = Some(received_at.clone());
        status.latest_activity = Some(event.clone());
        if status.running {
            status.detail = "Androidから状態を受信しています。".into();
        }
    }

    (StatusCode::OK, Json(json!({"ok":true,"stored":true,"received_at":received_at})))
}

async fn phone_health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let cfg = match config::load(&state.data_dir) {
        Ok(v) => v,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok":false}))),
    };
    if !authorized(&headers, &cfg.phone_receiver_token) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"ok":false,"error":"unauthorized"})));
    }
    (StatusCode::OK, Json(json!({"ok":true,"service":"yandere-companion-phone-receiver"})))
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    if token.trim().is_empty() { return false; }
    let expected = format!("Bearer {}", token.trim());
    headers.get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false)
}

fn normalize_event(event: &mut PhoneActivityEvent) {
    event.device_id = truncate(event.device_id.trim(), 128);
    event.device_name = truncate(event.device_name.trim(), 128);
    event.foreground_app = event.foreground_app.take().map(|s| truncate(s.trim(), 160)).filter(|s| !s.is_empty());
    event.foreground_package = event.foreground_package.take().map(|s| truncate(s.trim(), 220)).filter(|s| !s.is_empty());
    event.observed_at = event.observed_at.take().map(|s| truncate(s.trim(), 80)).filter(|s| !s.is_empty());
}

fn validate_event(event: &PhoneActivityEvent) -> Result<()> {
    if event.device_id.is_empty() { return Err(anyhow!("device_id is required")); }
    if event.device_name.is_empty() { return Err(anyhow!("device_name is required")); }
    Ok(())
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

async fn set_status(state: &Arc<AppState>, mut next: PhoneReceiverStatus) {
    // Preserve the latest device state across temporary Tailscale/server restarts.
    let current = state.phone_status.read().await.clone();
    if next.last_seen_at.is_none() { next.last_seen_at = current.last_seen_at; }
    if next.latest_activity.is_none() { next.latest_activity = current.latest_activity; }
    *state.phone_status.write().await = next;
}

pub async fn status(state: &Arc<AppState>) -> PhoneReceiverStatus {
    let mut status = state.phone_status.read().await.clone();
    if let Ok(Some((received_at, event))) = db::latest_phone_activity(&state.db_path) {
        status.last_seen_at = Some(received_at);
        status.latest_activity = Some(event);
    }
    status
}

fn detect_tailscale_ipv4() -> Option<Ipv4Addr> {
    // First prefer the official CLI when it is available in the user's login shell.
    if let Ok(out) = Command::new("/bin/zsh")
        .args(["-lc", "tailscale ip -4 2>/dev/null | head -n 1"])
        .output()
    {
        if out.status.success() {
            if let Ok(text) = String::from_utf8(out.stdout) {
                if let Ok(ip) = text.trim().parse::<Ipv4Addr>() {
                    if is_tailscale_ipv4(ip) { return Some(ip); }
                }
            }
        }
    }

    // Packaged GUI apps often have a smaller PATH. Fall back to finding the
    // CGNAT 100.64.0.0/10 address that Tailscale assigns to an interface.
    let out = Command::new("/sbin/ifconfig").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let words: Vec<&str> = text.split_whitespace().collect();
    for pair in words.windows(2) {
        if pair[0] == "inet" {
            if let Ok(ip) = pair[1].parse::<Ipv4Addr>() {
                if is_tailscale_ipv4(ip) { return Some(ip); }
            }
        }
    }
    None
}

fn is_tailscale_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}
