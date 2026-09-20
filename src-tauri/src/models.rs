use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppActivity {
    pub app: String,
    pub seconds: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapData {
    pub config: crate::config::AppConfig,
    pub messages: Vec<ChatMessage>,
    pub last_observation: Option<StoredObservation>,
    pub today_diary: Option<DiaryEntry>,
    pub screenpipe_ok: bool,
    pub rhythm: RhythmStatus,
    pub app_version: String,
    pub log_path: String,
    pub screenpipe_log_path: String,
}
