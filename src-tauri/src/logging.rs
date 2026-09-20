use anyhow::{Context, Result};
use chrono::Local;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

struct Logger {
    path: PathBuf,
    file: Mutex<File>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

pub fn init(data_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join("companion.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let _ = LOGGER.set(Logger {
        path: path.clone(),
        file: Mutex::new(file),
    });
    info(&format!("Yandere Companion {} starting", env!("CARGO_PKG_VERSION")));
    Ok(path)
}

pub fn path() -> Option<PathBuf> {
    LOGGER.get().map(|l| l.path.clone())
}

pub fn info(message: &str) { write("INFO", message); }
pub fn warn(message: &str) { write("WARN", message); }
pub fn error(message: &str) { write("ERROR", message); }

fn write(level: &str, message: &str) {
    let Some(logger) = LOGGER.get() else { return; };
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f %:z");
    let clean = message.replace('\n', "\\n");
    if let Ok(mut file) = logger.file.lock() {
        let _ = writeln!(file, "[{timestamp}] [{level}] {clean}");
        let _ = file.flush();
    }
}
