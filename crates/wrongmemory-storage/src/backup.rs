//! Consistent store backups (architecture §6, WM-600).
//!
//! Uses the SQLite online backup API from a READ-ONLY source connection, so
//! committed WAL content is included and the source is never modified
//! (copying `sage.db` alone would miss WAL frames). The snapshot is written
//! to a temporary sibling, checked with `PRAGMA integrity_check` and the
//! read-only inspector, then renamed into place. An existing destination is
//! never overwritten.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::backup::Backup;
use serde::Serialize;
use sha2::{Digest, Sha256};
use wrongmemory_contracts::{ErrorBody, ErrorCode};

use crate::inspect::{self, Verdict};
use crate::store::sql_err;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupReport {
    pub source: String,
    pub destination: String,
    pub bytes: u64,
    pub pages: i64,
    pub integrity: String,
    pub verdict: Verdict,
    pub memory_rows: Option<i64>,
    pub sha256: String,
}

fn io(e: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, e.to_string())
}

/// Snapshot `source` into `destination` (must not exist).
pub fn backup_store(source: &Path, destination: &Path) -> Result<BackupReport, ErrorBody> {
    if destination.exists() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidRequest,
            format!(
                "{} already exists; refusing to overwrite",
                destination.display()
            ),
        ));
    }
    let insp = inspect::inspect(source);
    if !insp.verdict.readable() {
        return Err(ErrorBody::new(
            ErrorCode::ProfileUnsupported,
            format!("{:?}: {}", insp.verdict, insp.detail),
        ));
    }
    let dir = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir).map_err(io)?;
    let tmp = dir.join(format!(
        ".{}.{}.partial",
        destination
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "backup".into()),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let result = (|| -> Result<BackupReport, ErrorBody> {
        let src = inspect::open_read_only(source).map_err(sql_err)?;
        {
            let mut dst = Connection::open(&tmp).map_err(sql_err)?;
            let b = Backup::new(&src, &mut dst).map_err(sql_err)?;
            b.run_to_completion(256, Duration::from_millis(5), None)
                .map_err(sql_err)?;
        }
        drop(src);
        // The copy is a standalone database: switch it out of WAL so the
        // single file is self-contained, then verify it.
        let (integrity, pages) = {
            let c = Connection::open(&tmp).map_err(sql_err)?;
            c.pragma_update(None, "journal_mode", "DELETE")
                .map_err(sql_err)?;
            let integrity: String = c
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .map_err(sql_err)?;
            let pages: i64 = c
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .map_err(sql_err)?;
            (integrity, pages)
        };
        if integrity != "ok" {
            return Err(ErrorBody::new(
                ErrorCode::DatabaseCorrupt,
                format!("backup failed integrity_check: {integrity}"),
            ));
        }
        let copy = inspect::inspect(&tmp);
        let bytes = std::fs::read(&tmp).map_err(io)?;
        let sha = hex::encode(Sha256::digest(&bytes));
        Ok(BackupReport {
            source: source.display().to_string(),
            destination: destination.display().to_string(),
            bytes: bytes.len() as u64,
            pages,
            integrity,
            verdict: copy.verdict,
            memory_rows: copy.memory_rows,
            sha256: sha,
        })
    })();
    match result {
        Ok(report) => {
            // Never clobber a destination that appeared meanwhile.
            if destination.exists() {
                let _ = std::fs::remove_file(&tmp);
                return Err(ErrorBody::new(
                    ErrorCode::InvalidRequest,
                    format!(
                        "{} appeared during backup; refusing to overwrite",
                        destination.display()
                    ),
                ));
            }
            std::fs::rename(&tmp, destination).map_err(io)?;
            Ok(report)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MutationCtx, SageStore, Viewer};
    use wrongmemory_contracts::RememberParams;

    const NOW: &str = "2026-11-01T12:00:00.000Z";

    #[test]
    fn backup_includes_uncheckpointed_wal_and_refuses_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p").join("sage.db");
        SageStore::create_managed(&path, NOW).unwrap();
        let (mut store, _) = SageStore::open(&path, true, None, NOW).unwrap();
        let ctx = MutationCtx {
            principal_id: "host".into(),
            client_label: "t".into(),
            session_id: None,
            idempotency_key: None,
            now_iso: NOW.into(),
        };
        let id = store
            .remember(
                &RememberParams {
                    text: "Backups must include WAL frames".into(),
                    ..Default::default()
                },
                &ctx,
            )
            .unwrap()
            .id;
        // The writer stays open: the new row lives in the WAL only.
        let out = tmp.path().join("b").join("snap.db");
        let r = backup_store(&path, &out).unwrap();
        assert_eq!(r.integrity, "ok");
        assert_eq!(r.verdict, Verdict::Compatible);
        assert_eq!(r.memory_rows, Some(1));
        let (copy, _) = SageStore::open(&out, false, None, NOW).unwrap();
        assert!(copy.get(&id, &Viewer::default()).unwrap().is_some());
        assert!(backup_store(&path, &out).is_err());
        drop(store);
    }
}
