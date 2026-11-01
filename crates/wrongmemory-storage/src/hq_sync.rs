//! HQ sync projection (`sqlite-store-hq-sync.ts` at the baseline).
//!
//! The projection table `hq_memory_sync` and its triggers are part of the
//! SAGE v5 schema (`schema.rs`): every change to a shared-scope memory
//! (`project`, `file`, `symbol`) re-versions its row with a fresh change id,
//! without local access counters. This module serves the three operations
//! the HQ publisher uses: the clock version, a paged list, and applying
//! remote records inside one writer transaction without echoing them.
//!
//! Session-scoped memories are never projected, and a remote record can
//! never overwrite a local memory outside the shared scopes.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use wrongmemory_contracts::{ErrorBody, ErrorCode};

use crate::mutate::{cascade_delete_edges, clean_referencing, sync_edges, upsert_raw};
use crate::store::{MutationCtx, SageStore, StoreResult, audit, sql_err};

const SHARED_SCOPES: [&str; 3] = ["project", "file", "symbol"];
/// `MAX_HQ_SAGE_RECORD_BYTES`.
const MAX_RECORD_BYTES: usize = 256 * 1024;
const MAX_RECORDS: usize = 100;
const PAGE_ROWS: i64 = 100;
const PAGE_BYTES: usize = 512 * 1024;

fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `isHqSageRecord`.
pub fn is_hq_sage_record(v: &Value) -> bool {
    let Some(r) = v.as_object() else { return false };
    let Some(id) = r.get("id").and_then(Value::as_str) else {
        return false;
    };
    if id.is_empty() || js_len(id) > 256 {
        return false;
    }
    let revision_ok = r.get("revision").is_some_and(|n| {
        n.as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            .is_some_and(|x| (1..=9_007_199_254_740_991).contains(&x))
    });
    let change_ok = r.get("changeId").and_then(Value::as_str).is_some_and(|c| {
        c.len() == 32
            && c.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    if !revision_ok || !change_ok {
        return false;
    }
    let text = v.to_string();
    if js_len(&text) > MAX_RECORD_BYTES || text.len() > MAX_RECORD_BYTES {
        return false;
    }
    let m = match r.get("memory") {
        None => return false,
        Some(Value::Null) => return true,
        Some(Value::Object(m)) => m,
        Some(_) => return false,
    };
    if m.get("id").and_then(Value::as_str) != Some(id)
        || !m
            .get("scope")
            .and_then(Value::as_str)
            .is_some_and(|s| SHARED_SCOPES.contains(&s))
    {
        return false;
    }
    let strings_ok = ["text", "kind", "status", "createdAt", "updatedAt"]
        .iter()
        .all(|k| m.get(*k).is_some_and(Value::is_string));
    let numbers_ok = ["importance", "confidence", "freshness"].iter().all(|k| {
        m.get(*k)
            .and_then(Value::as_f64)
            .is_some_and(f64::is_finite)
    });
    let tags_ok = m
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().all(Value::is_string));
    let typed = |key: &str| {
        m.get(key).and_then(Value::as_array).is_some_and(|a| {
            a.iter()
                .all(|x| x.get("type").is_some_and(Value::is_string))
        })
    };
    strings_ok && numbers_ok && tags_ok && typed("anchors") && typed("sources")
}

fn deleted_like(r: &Value) -> bool {
    r["memory"].is_null() || r["memory"]["status"] == "deleted"
}

/// `compareHqSageRecords`: revision, then deletes win ties, then change id.
pub fn compare_hq_records(a: &Value, b: &Value) -> std::cmp::Ordering {
    let rev = |r: &Value| r["revision"].as_f64().unwrap_or(0.0);
    rev(a)
        .partial_cmp(&rev(b))
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| deleted_like(a).cmp(&deleted_like(b)))
        .then_with(|| {
            a["changeId"]
                .as_str()
                .unwrap_or("")
                .cmp(b["changeId"].as_str().unwrap_or(""))
        })
}

fn projection_record(id: String, revision: i64, change_id: String, data: Option<String>) -> Value {
    json!({
        "id": id,
        "revision": revision,
        "changeId": change_id,
        "memory": data.and_then(|d| serde_json::from_str::<Value>(&d).ok()).unwrap_or(Value::Null),
    })
}

impl SageStore {
    /// `getHqSyncVersion`: `<epoch>:<clock revision>`.
    pub fn hq_sync_version(&self) -> StoreResult<String> {
        let (epoch, revision): (String, i64) = self
            .conn
            .query_row(
                "SELECT epoch, revision FROM hq_memory_sync_clock WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql_err)?;
        Ok(format!("{epoch}:{revision}"))
    }

    /// `listHqSync(after)`: projection rows with `id > after`, in id order,
    /// at most 100 per page and about 512 KiB.
    pub fn list_hq_sync(&self, after: &str) -> StoreResult<Vec<Value>> {
        let mut st = self
            .conn
            .prepare("SELECT id, revision, change_id, data FROM hq_memory_sync WHERE id > ?1 ORDER BY id LIMIT ?2")
            .map_err(sql_err)?;
        let rows = st
            .query_map(params![after, PAGE_ROWS], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        let mut page = Vec::new();
        let mut bytes = 0usize;
        for (id, revision, change, data) in rows {
            let size = data.as_deref().map_or(0, str::len) + 512;
            if !page.is_empty() && bytes + size > PAGE_BYTES {
                break;
            }
            bytes += size;
            page.push(projection_record(id, revision, change, data));
        }
        Ok(page)
    }

    /// `applyHqSync(records)`: apply newer remote records in one writer
    /// transaction. The projection row is set to the remote version
    /// afterwards, so an import never re-versions or echoes. Returns the
    /// changed ids (each audited as `memory.hq_synced`).
    pub fn apply_hq_sync(
        &mut self,
        records: &[Value],
        ctx: &MutationCtx,
    ) -> StoreResult<Vec<String>> {
        self.require_writable()?;
        if records.len() > MAX_RECORDS || !records.iter().all(is_hq_sage_record) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidRequest,
                "Invalid SAGE sync records",
            ));
        }
        let now = ctx.now_iso.clone();
        self.in_transaction(|s| {
            let conn = &s.conn;
            let mut changed = Vec::new();
            let mut deleted: HashSet<String> = HashSet::new();
            for r in records {
                let id = r["id"].as_str().unwrap_or_default().to_string();
                let prior = conn
                    .query_row(
                        "SELECT id, revision, change_id, data FROM hq_memory_sync WHERE id = ?1",
                        [&id],
                        |x| {
                            Ok(projection_record(
                                x.get(0)?,
                                x.get(1)?,
                                x.get(2)?,
                                x.get(3)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(sql_err)?;
                if prior.is_some_and(|p| compare_hq_records(r, &p).is_le()) {
                    continue;
                }
                let scope: Option<String> = conn
                    .query_row("SELECT scope FROM memories WHERE id = ?1", [&id], |x| {
                        x.get(0)
                    })
                    .optional()
                    .map_err(sql_err)?;
                // A remote id collision cannot overwrite private local memory.
                if scope.is_some_and(|sc| !SHARED_SCOPES.contains(&sc.as_str())) {
                    continue;
                }
                let node = format!("mem:{id}");
                match &r["memory"] {
                    Value::Null => {
                        conn.execute("DELETE FROM memories WHERE id = ?1", [&id])
                            .map_err(sql_err)?;
                        cascade_delete_edges(conn, &node)?;
                        deleted.insert(id.clone());
                    }
                    Value::Object(m) => {
                        let mut m: Map<String, Value> = m.clone();
                        if m.get("status").and_then(Value::as_str) == Some("deleted") {
                            for k in ["supersedes", "contradicts", "supersededBy"] {
                                m.remove(k);
                            }
                            upsert_raw(conn, &mut m)?;
                            cascade_delete_edges(conn, &node)?;
                            deleted.insert(id.clone());
                        } else {
                            let rec = upsert_raw(conn, &mut m)?;
                            sync_edges(conn, &rec, &now)?;
                        }
                    }
                    _ => unreachable!("validated"),
                }
                conn.execute(
                    "INSERT INTO hq_memory_sync VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(id) DO UPDATE SET revision = excluded.revision,
                       change_id = excluded.change_id, data = excluded.data",
                    params![
                        id,
                        r["revision"].as_i64().unwrap_or(1),
                        r["changeId"].as_str().unwrap_or_default(),
                        match &r["memory"] {
                            Value::Null => None,
                            m => Some(m.to_string()),
                        }
                    ],
                )
                .map_err(sql_err)?;
                changed.push(id);
            }
            if !deleted.is_empty() {
                clean_referencing(conn, &deleted, &now)?;
            }
            for id in &changed {
                audit(conn, "memory.hq_synced", &now, json!({"memoryId": id})).map_err(sql_err)?;
            }
            Ok(changed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str) -> Value {
        let now = "2026-11-01T00:00:00.000Z";
        json!({"id": id, "revision": 99, "changeId": "a".repeat(32), "memory": {
            "id": id, "scope": "project", "text": "t", "kind": "fact", "status": "active",
            "createdAt": now, "updatedAt": now, "importance": 0.5, "confidence": 0.5, "freshness": 1,
            "tags": [], "anchors": [], "sources": [{"type": "user"}], "revision": 1, "schemaVersion": 1}})
    }

    #[test]
    fn record_validation_follows_is_hq_sage_record() {
        assert!(is_hq_sage_record(&rec("01ABC")));
        assert!(is_hq_sage_record(
            &json!({"id": "x", "revision": 1, "changeId": "0".repeat(32), "memory": null})
        ));
        let mut bad = rec("01ABC");
        bad["changeId"] = json!("A".repeat(32));
        assert!(!is_hq_sage_record(&bad));
        let mut bad = rec("01ABC");
        bad["memory"]["scope"] = json!("session");
        assert!(!is_hq_sage_record(&bad));
        let mut bad = rec("01ABC");
        bad["memory"]["id"] = json!("other");
        assert!(!is_hq_sage_record(&bad));
        let mut bad = rec("01ABC");
        bad["revision"] = json!(0);
        assert!(!is_hq_sage_record(&bad));
        assert!(!is_hq_sage_record(
            &json!({"id": "x", "revision": 1, "changeId": "0".repeat(32)})
        ));
    }
}
