use crate::models::{ActivitySnapshot, ChatMessage, DiaryEntry, ObservationDecision, StoredObservation};
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
