//! Minimal structured logging: one JSON object per line on stderr, plus a
//! bounded file under `<data-root>/logs`. Never logs tokens or source text.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::{Value, json};

/// The log file is rotated (single `.1` backup) when it exceeds this size.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

static SINK: OnceLock<Mutex<Option<LogFile>>> = OnceLock::new();

struct LogFile {
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
}

impl LogFile {
    fn open(path: PathBuf) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            path,
            file: Some(file),
            bytes,
        })
    }

    fn append(&mut self, line: &str) -> io::Result<()> {
        let added = line.len() as u64 + 1;
        // Keep oversized events on stderr without splitting a JSON record or
        // allowing one event to defeat the file quota.
        if added > MAX_LOG_BYTES {
            return Ok(());
        }
        if self.bytes.saturating_add(added) > MAX_LOG_BYTES {
            // Windows cannot rename an open file. The mutex covers closing,
            // replacing the one backup, reopening, and the complete append.
            self.file.take();
            let backup = self.path.with_extension("log.1");
            match std::fs::remove_file(&backup) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            std::fs::rename(&self.path, backup)?;
            self.bytes = 0;
        }
        if self.file.is_none() {
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }
        let file = self.file.as_mut().expect("log opened above");
        match writeln!(file, "{line}") {
            Ok(()) => {
                self.bytes += added;
                Ok(())
            }
            Err(e) => {
                // Include a partial write in the next quota check after an I/O error.
                self.bytes = file.metadata()?.len();
                Err(e)
            }
        }
    }
}

pub fn init(logs_dir: &Path) {
    let file = LogFile::open(logs_dir.join("wrongmemoryd.log")).ok();
    let _ = SINK.set(Mutex::new(file));
}

pub fn event(level: &str, msg: &str, fields: Value) {
    let line = json!({
        "ts": wrongmemory_registry::now_ms(),
        "level": level,
        "msg": msg,
        "pid": std::process::id(),
        "fields": fields,
    })
    .to_string();
    eprintln!("{line}");
    if let Some(sink) = SINK.get()
        && let Ok(mut guard) = sink.lock()
        && let Some(f) = guard.as_mut()
    {
        let _ = f.append(&line);
    }
}

#[macro_export]
macro_rules! info {
    ($msg:expr) => { $crate::log::event("info", $msg, serde_json::Value::Null) };
    ($msg:expr, $($fields:tt)+) => { $crate::log::event("info", $msg, serde_json::json!($($fields)+)) };
}

#[macro_export]
macro_rules! warn {
    ($msg:expr) => { $crate::log::event("warn", $msg, serde_json::Value::Null) };
    ($msg:expr, $($fields:tt)+) => { $crate::log::event("warn", $msg, serde_json::json!($($fields)+)) };
}

#[cfg(test)]
mod tests {
    #[test]
    fn running_daemon_rotates_logs_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        super::init(dir.path());
        let path = dir.path().join("wrongmemoryd.log");
        super::event("info", "control", serde_json::json!({}));
        assert!(std::fs::metadata(&path).unwrap().len() < super::MAX_LOG_BYTES);
        // Cross the quota twice, including replacement of an existing backup.
        let padding = "x".repeat(32 * 1024);
        for _ in 0..340 {
            super::event("info", "fixture", serde_json::json!({"padding": padding}));
        }
        let actual = std::fs::metadata(&path).unwrap().len();
        assert!(
            actual <= super::MAX_LOG_BYTES,
            "EXPECTED: active log <= {} bytes; ACTUAL: {actual}",
            super::MAX_LOG_BYTES
        );
        let backup = dir.path().join("wrongmemoryd.log.1");
        assert!(std::fs::metadata(&backup).unwrap().len() <= super::MAX_LOG_BYTES);
        for file in [&path, &backup] {
            let text = std::fs::read_to_string(file).unwrap();
            assert!(!text.is_empty());
            for line in text.lines() {
                serde_json::from_str::<serde_json::Value>(line).unwrap();
            }
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        // Release the global file handle before TempDir removes it on Windows.
        super::SINK.get().unwrap().lock().unwrap().take();
    }

    #[test]
    fn log_quota_keeps_whole_records_at_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrongmemoryd.log");
        let mut sink = super::LogFile::open(path.clone()).unwrap();
        sink.append("").unwrap();
        sink.append(&"é".repeat((super::MAX_LOG_BYTES as usize - 2) / 2))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            super::MAX_LOG_BYTES
        );
        // A single oversized record must not change either retained file.
        sink.append(&"x".repeat(super::MAX_LOG_BYTES as usize))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            super::MAX_LOG_BYTES
        );
        assert!(!dir.path().join("wrongmemoryd.log.1").exists());
        sink.append("next").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "next\n");
        assert_eq!(
            std::fs::metadata(dir.path().join("wrongmemoryd.log.1"))
                .unwrap()
                .len(),
            super::MAX_LOG_BYTES
        );
    }

    #[test]
    fn startup_rotation_replaces_an_existing_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrongmemoryd.log");
        let backup = dir.path().join("wrongmemoryd.log.1");
        std::fs::write(&path, vec![b'x'; super::MAX_LOG_BYTES as usize]).unwrap();
        std::fs::write(&backup, "old backup").unwrap();
        let mut sink = super::LogFile::open(path.clone()).unwrap();
        sink.append("new run").unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new run\n");
        assert_eq!(
            std::fs::metadata(backup).unwrap().len(),
            super::MAX_LOG_BYTES
        );
    }
}
