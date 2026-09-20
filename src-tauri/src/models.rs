use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppActivity {
    pub app: String,
    pub seconds: u64,
}


#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PhoneActivityEvent {
    pub device_id: String,
    pub device_name: String,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub interactive: bool,
    #[serde(default)]
    pub unlocked: bool,
    #[serde(default)]
    pub foreground_app: Option<String>,
    #[serde(default)]
    pub foreground_package: Option<String>,
    #[serde(default)]
    pub session_seconds: Option<u64>,
    #[serde(default)]
    pub last_interaction_seconds_ago: Option<u64>,
    #[serde(default)]
    pub clicks_1m: Option<u32>,
    #[serde(default)]
    pub scrolls_1m: Option<u32>,
    #[serde(default)]
    pub app_switches_5m: Option<u32>,
    #[serde(default)]
    pub pickups_20m: Option<u32>,
    #[serde(default)]
    pub phone_minutes_20m: Option<f32>,
    #[serde(default)]
    pub longest_session_seconds_20m: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PhoneActivitySummary {
    pub last_seen_at: String,
    pub connected_recently: bool,
    pub device_name: String,
    pub interactive: bool,
    pub unlocked: bool,
    pub foreground_app: Option<String>,
    pub foreground_package: Option<String>,
    pub session_seconds: Option<u64>,
    pub last_interaction_seconds_ago: Option<u64>,
    pub clicks_1m: Option<u32>,
    pub scrolls_1m: Option<u32>,
    pub app_switches_5m: Option<u32>,
    pub pickups_20m: Option<u32>,
    pub phone_minutes_20m: Option<f32>,
    pub longest_session_seconds_20m: Option<u64>,
    pub samples_in_window: usize,
    pub observed_apps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PhoneReceiverStatus {
    pub enabled: bool,
    pub running: bool,
    pub tailscale_ip: Option<String>,
    pub port: u16,
    pub endpoint: String,
    pub detail: String,
    pub last_seen_at: Option<String>,
    pub latest_activity: Option<PhoneActivityEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActivitySnapshot {
    pub start_time: String,
    pub end_time: String,
    pub foreground_apps: Vec<AppActivity>,
    pub window_titles: Vec<String>,
    pub keyboard_events: usize,
    pub mouse_events: usize,
    pub input_events_total: usize,
    pub observed_records: usize,
    pub idle_seconds_estimate: u64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub transient_text_snippets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub phone: Option<PhoneActivitySummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationDecision {
    pub activity: String,
    pub working: bool,
    pub focus_level: f32,
    pub summary: String,
    pub active_app: String,
    pub mood: String,
    pub should_speak: bool,
    pub speak_reason: String,
    pub intent: String,
    pub notification_hint: String,
}

impl Default for ObservationDecision {
    fn default() -> Self {
        Self {
            activity: "unknown".into(),
            working: false,
            focus_level: 0.0,
            summary: "まだ十分な観察情報がありません。".into(),
            active_app: "".into(),
            mood: "calm".into(),
            should_speak: false,
            speak_reason: "insufficient_data".into(),
            intent: "none".into(),
            notification_hint: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredObservation {
    pub id: i64,
    pub created_at: String,
    pub snapshot: ActivitySnapshot,
    pub decision: ObservationDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: i64,
    pub created_at: String,
    pub role: String,
    pub text: String,
    pub proactive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiaryEntry {
    pub date: String,
    pub created_at: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RhythmStatus {
    pub sleeping: bool,
    pub sleep_at_local: String,
    #[serde(skip_serializing)]
    pub wake_at_local: String,
    pub sleep_at_display: String,
    #[serde(skip_serializing)]
    pub wake_at_display: String,
}


#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScreenpipeHealth {
    pub reachable: bool,
    pub screen_capture_ok: bool,
    pub accessibility_ok: bool,
    pub input_monitoring_ok: bool,
    pub ui_recorder_running: bool,
    pub events_inserted: u64,
    pub frame_status: String,
    pub vision_reason: String,
    pub ui_mode: String,
    pub screenpipe_version: Option<String>,
    pub detail: String,
}

impl ScreenpipeHealth {
    pub fn observation_ready(&self) -> bool {
        self.reachable
            && self.screen_capture_ok
            && self.accessibility_ok
            && self.input_monitoring_ok
            && self.ui_recorder_running
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapData {
    pub config: crate::config::AppConfig,
    pub messages: Vec<ChatMessage>,
    pub last_observation: Option<StoredObservation>,
    pub today_diary: Option<DiaryEntry>,
    pub screenpipe_ok: bool,
    pub screenpipe_health: ScreenpipeHealth,
    pub phone_receiver: PhoneReceiverStatus,
    pub rhythm: RhythmStatus,
    pub app_version: String,
    pub log_path: String,
    pub screenpipe_log_path: String,
}
