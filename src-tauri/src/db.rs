use crate::models::{ActivitySnapshot, ChatMessage, DiaryEntry, ObservationDecision, PhoneActivityEvent, PhoneActivitySummary, StoredObservation};
use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::Path;

pub fn init(path: &Path) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch(r#"
        PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS observations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            created_at TEXT NOT NULL,
            snapshot_json TEXT NOT NULL,
            decision_json TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            created_at TEXT NOT NULL,
            role TEXT NOT NULL,
            text TEXT NOT NULL,
            proactive INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS diaries (
            date TEXT PRIMARY KEY,
            created_at TEXT NOT NULL,
            text TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS phone_activity (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            received_at TEXT NOT NULL,
            device_id TEXT NOT NULL,
            device_name TEXT NOT NULL,
            observed_at TEXT,
            interactive INTEGER NOT NULL,
            unlocked INTEGER NOT NULL,
            foreground_app TEXT,
            foreground_package TEXT,
            session_seconds INTEGER,
            last_interaction_seconds_ago INTEGER,
            clicks_1m INTEGER,
            scrolls_1m INTEGER,
            app_switches_5m INTEGER,
            pickups_20m INTEGER,
            phone_minutes_20m REAL,
            longest_session_seconds_20m INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_phone_activity_received_at ON phone_activity(received_at);
    "#)?;
    Ok(())
}

pub fn insert_observation(path: &Path, created_at: &str, snapshot: &ActivitySnapshot, decision: &ObservationDecision) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute(
        "INSERT INTO observations(created_at,snapshot_json,decision_json) VALUES (?1,?2,?3)",
        params![created_at, serde_json::to_string(snapshot)?, serde_json::to_string(decision)?],
    )?;
    Ok(())
}

pub fn recent_observations(path: &Path, limit: usize) -> Result<Vec<StoredObservation>> {
    let conn = Connection::open(path)?;
    let mut st = conn.prepare("SELECT id,created_at,snapshot_json,decision_json FROM observations ORDER BY id DESC LIMIT ?1")?;
    let mut rows = st.query(params![limit as i64])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(StoredObservation {
            id: r.get(0)?,
            created_at: r.get(1)?,
            snapshot: serde_json::from_str::<ActivitySnapshot>(&r.get::<_, String>(2)?)?,
            decision: serde_json::from_str::<ObservationDecision>(&r.get::<_, String>(3)?)?,
        });
    }
    out.reverse();
    Ok(out)
}

pub fn observations_since(path: &Path, since: &str) -> Result<Vec<StoredObservation>> {
    let conn = Connection::open(path)?;
    let mut st = conn.prepare("SELECT id,created_at,snapshot_json,decision_json FROM observations WHERE created_at >= ?1 ORDER BY id ASC")?;
    let rows = st.query_map(params![since], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, created_at, s, d) = row?;
        out.push(StoredObservation { id, created_at, snapshot: serde_json::from_str(&s)?, decision: serde_json::from_str(&d)? });
    }
    Ok(out)
}

pub fn insert_message(path: &Path, created_at: &str, role: &str, text: &str, proactive: bool) -> Result<i64> {
    let conn = Connection::open(path)?;
    conn.execute("INSERT INTO messages(created_at,role,text,proactive) VALUES (?1,?2,?3,?4)", params![created_at, role, text, proactive as i32])?;
    Ok(conn.last_insert_rowid())
}

pub fn recent_messages(path: &Path, limit: usize) -> Result<Vec<ChatMessage>> {
    let conn = Connection::open(path)?;
    let mut st = conn.prepare("SELECT id,created_at,role,text,proactive FROM messages ORDER BY id DESC LIMIT ?1")?;
    let rows = st.query_map(params![limit as i64], |r| Ok(ChatMessage { id:r.get(0)?, created_at:r.get(1)?, role:r.get(2)?, text:r.get(3)?, proactive:r.get::<_, i32>(4)? != 0 }))?;
    let mut out: Vec<_> = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    out.reverse();
    Ok(out)
}

pub fn notification_count_since(path: &Path, since: &str) -> Result<usize> {
    let conn = Connection::open(path)?;
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM messages WHERE proactive=1 AND created_at >= ?1", params![since], |r| r.get(0))?;
    Ok(n as usize)
}

pub fn last_proactive_time(path: &Path) -> Result<Option<String>> {
    let conn = Connection::open(path)?;
    let mut st = conn.prepare("SELECT created_at FROM messages WHERE proactive=1 ORDER BY id DESC LIMIT 1")?;
    let mut rows = st.query([])?;
    Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
}

pub fn save_diary(path: &Path, date: &str, created_at: &str, text: &str) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute("INSERT INTO diaries(date,created_at,text) VALUES (?1,?2,?3) ON CONFLICT(date) DO UPDATE SET created_at=excluded.created_at,text=excluded.text", params![date,created_at,text])?;
    Ok(())
}

pub fn get_diary(path: &Path, date: &str) -> Result<Option<DiaryEntry>> {
    let conn = Connection::open(path)?;
    let mut st = conn.prepare("SELECT date,created_at,text FROM diaries WHERE date=?1")?;
    let mut rows = st.query(params![date])?;
    if let Some(r) = rows.next()? {
        Ok(Some(DiaryEntry {
            date: r.get(0)?,
            created_at: r.get(1)?,
            text: r.get(2)?,
        }))
    } else {
        Ok(None)
    }
}


pub fn insert_phone_activity(path: &Path, received_at: &str, event: &PhoneActivityEvent) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute(
        r#"INSERT INTO phone_activity(
            received_at, device_id, device_name, observed_at, interactive, unlocked,
            foreground_app, foreground_package, session_seconds, last_interaction_seconds_ago,
            clicks_1m, scrolls_1m, app_switches_5m, pickups_20m, phone_minutes_20m, longest_session_seconds_20m
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"#,
        params![
            received_at,
            event.device_id,
            event.device_name,
            event.observed_at,
            event.interactive as i32,
            event.unlocked as i32,
            event.foreground_app,
            event.foreground_package,
            event.session_seconds.map(|v| v as i64),
            event.last_interaction_seconds_ago.map(|v| v as i64),
            event.clicks_1m.map(|v| v as i64),
            event.scrolls_1m.map(|v| v as i64),
            event.app_switches_5m.map(|v| v as i64),
            event.pickups_20m.map(|v| v as i64),
            event.phone_minutes_20m.map(|v| v as f64),
            event.longest_session_seconds_20m.map(|v| v as i64),
        ],
    )?;
    Ok(())
}

fn phone_event_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, PhoneActivityEvent)> {
    Ok((
        r.get(0)?,
        PhoneActivityEvent {
            device_id: r.get(1)?,
            device_name: r.get(2)?,
            observed_at: r.get(3)?,
            interactive: r.get::<_, i32>(4)? != 0,
            unlocked: r.get::<_, i32>(5)? != 0,
            foreground_app: r.get(6)?,
            foreground_package: r.get(7)?,
            session_seconds: r.get::<_, Option<i64>>(8)?.map(|v| v.max(0) as u64),
            last_interaction_seconds_ago: r.get::<_, Option<i64>>(9)?.map(|v| v.max(0) as u64),
            clicks_1m: r.get::<_, Option<i64>>(10)?.map(|v| v.max(0) as u32),
            scrolls_1m: r.get::<_, Option<i64>>(11)?.map(|v| v.max(0) as u32),
            app_switches_5m: r.get::<_, Option<i64>>(12)?.map(|v| v.max(0) as u32),
            pickups_20m: r.get::<_, Option<i64>>(13)?.map(|v| v.max(0) as u32),
            phone_minutes_20m: r.get::<_, Option<f64>>(14)?.map(|v| v as f32),
            longest_session_seconds_20m: r.get::<_, Option<i64>>(15)?.map(|v| v.max(0) as u64),
        },
    ))
}

pub fn latest_phone_activity(path: &Path) -> Result<Option<(String, PhoneActivityEvent)>> {
    let conn = Connection::open(path)?;
    let mut st = conn.prepare(
        r#"SELECT received_at, device_id, device_name, observed_at, interactive, unlocked,
                  foreground_app, foreground_package, session_seconds, last_interaction_seconds_ago,
                  clicks_1m, scrolls_1m, app_switches_5m, pickups_20m, phone_minutes_20m, longest_session_seconds_20m
           FROM phone_activity
           ORDER BY id DESC
           LIMIT 1"#
    )?;
    let mut rows = st.query([])?;
    if let Some(row) = rows.next()? {
        Ok(Some(phone_event_from_row(row)?))
    } else {
        Ok(None)
    }
}

pub fn phone_summary_since(path: &Path, since: &str) -> Result<Option<PhoneActivitySummary>> {
    use std::collections::BTreeSet;
    let conn = Connection::open(path)?;
    let mut st = conn.prepare(
        r#"SELECT received_at, device_id, device_name, observed_at, interactive, unlocked,
                  foreground_app, foreground_package, session_seconds, last_interaction_seconds_ago,
                  clicks_1m, scrolls_1m, app_switches_5m, pickups_20m, phone_minutes_20m, longest_session_seconds_20m
           FROM phone_activity
           WHERE received_at >= ?1
           ORDER BY id ASC"#
    )?;
    let mut rows = st.query(params![since])?;
    let mut latest: Option<(String, PhoneActivityEvent)> = None;
    let mut samples = 0usize;
    let mut apps = BTreeSet::new();
    while let Some(row) = rows.next()? {
        let item = phone_event_from_row(row)?;
        if let Some(app) = item.1.foreground_app.as_deref() {
            if !app.trim().is_empty() { apps.insert(app.to_string()); }
        }
        samples += 1;
        latest = Some(item);
    }
    let Some((last_seen_at, event)) = latest else { return Ok(None); };
    let connected_recently = chrono::DateTime::parse_from_rfc3339(&last_seen_at)
        .map(|t| chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc)).num_seconds() <= 120)
        .unwrap_or(false);
    Ok(Some(PhoneActivitySummary {
        last_seen_at,
        connected_recently,
        device_name: event.device_name,
        interactive: event.interactive,
        unlocked: event.unlocked,
        foreground_app: event.foreground_app,
        foreground_package: event.foreground_package,
        session_seconds: event.session_seconds,
        last_interaction_seconds_ago: event.last_interaction_seconds_ago,
        clicks_1m: event.clicks_1m,
        scrolls_1m: event.scrolls_1m,
        app_switches_5m: event.app_switches_5m,
        pickups_20m: event.pickups_20m,
        phone_minutes_20m: event.phone_minutes_20m,
        longest_session_seconds_20m: event.longest_session_seconds_20m,
        samples_in_window: samples,
        observed_apps: apps.into_iter().take(12).collect(),
    }))
}
