//! SAGE Doctor (`runSqliteSageDoctor`, `sqlite-store-doctor.ts` at the
//! baseline) and `readAudit`.
//!
//! A read-mostly health report over the whole store plus the closed set of
//! named, idempotent repairs. The report covers every session's records, so
//! the daemon serves it to the host principal only.
//!
//! Deliberate difference: the `audience-runtime-mode` repair patches the
//! stored JSON object instead of re-encoding a typed view (AGENTS.md), so
//! unknown fields and `null` values survive.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use wrongmemory_contracts::{ErrorBody, ErrorCode};

use crate::hygiene::HygieneOptions;
use crate::mutate::{read_raw, upsert_raw};
use crate::store::{MutationCtx, SageStore, StoreResult, audit, sql_err};

/// Injection count from which a never-used memory is reported (metrics).
const NEVER_USED_REPORT_MIN_INJECTIONS: i64 = 20;
const PENDING_CANDIDATE_WARN: i64 = 200;
const WAL_WARN_BYTES: u64 = 64 * 1024 * 1024;
const SAMPLE_LIMIT: usize = 5;
const RUNTIME_DEFAULT_MODE: &str = "default";
/// `GRAPH_HUB_DEGREE` / `SOFT_RELATIONS` (sqlite-store-graph-traverse.ts).
const GRAPH_HUB_DEGREE: i64 = 64;
const SOFT_RELATIONS: [&str; 2] = ["same_topic", "related_to"];

/// The repairs `doctor({ fix })` accepts (`SAGE_DOCTOR_FIX_IDS`).
pub const DOCTOR_FIX_IDS: [&str; 5] = [
    "hygiene",
    "graph-hubs",
    "dangling-edges",
    "audience-runtime-mode",
    "fts-rebuild",
];

const HUB_NODES_SQL: &str = "
  SELECT node AS v, COUNT(*) AS n FROM (
    SELECT from_node AS node FROM edges WHERE relation IN ('same_topic', 'related_to')
    UNION ALL
    SELECT to_node AS node FROM edges WHERE relation IN ('same_topic', 'related_to')
  ) GROUP BY node HAVING COUNT(*) > ?1 ORDER BY n DESC";

const DANGLING_EDGE_FILTER: &str = "
  (from_node LIKE 'mem:%' AND NOT EXISTS (
     SELECT 1 FROM memories m WHERE m.id = substr(edges.from_node, 5) AND m.status <> 'deleted'))
  OR (to_node LIKE 'mem:%' AND NOT EXISTS (
     SELECT 1 FROM memories m WHERE m.id = substr(edges.to_node, 5) AND m.status <> 'deleted'))";

const RUNTIME_MODE_AUDIENCE_SQL: &str = "
  SELECT id AS v FROM memories
  WHERE status IN ('active', 'stale') AND audience IS NOT NULL AND audience <> ''
    AND json_valid(audience)
    AND EXISTS (SELECT 1 FROM json_each(json_extract(audience, '$.modes')) WHERE value = ?1)";

/// JS `Number` rendering: whole values as integers.
fn num(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

fn count(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> StoreResult<i64> {
    let n: Option<f64> = conn.query_row(sql, p, |r| r.get(0)).map_err(sql_err)?;
    Ok(n.unwrap_or(0.0) as i64)
}

fn strings(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> StoreResult<Vec<String>> {
    let mut st = conn.prepare(sql).map_err(sql_err)?;
    let rows = st
        .query_map(p, |r| r.get::<_, String>(0))
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    Ok(rows)
}

fn hubs(conn: &Connection) -> StoreResult<Vec<(String, i64)>> {
    let mut st = conn.prepare(HUB_NODES_SQL).map_err(sql_err)?;
    let rows = st
        .query_map([GRAPH_HUB_DEGREE], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    Ok(rows)
}

fn file_bytes(p: &std::path::Path) -> Option<u64> {
    std::fs::metadata(p).ok().map(|m| m.len())
}

/// `check()`: `ok` with count 0, else the given severity and extras.
fn check(id: &str, title: &str, n: i64, severity: &str, summary: String, extra: Value) -> Value {
    if n == 0 {
        return json!({"id": id, "title": title, "severity": "ok", "summary": format!("{title}: none"), "count": 0});
    }
    let mut v =
        json!({"id": id, "title": title, "severity": severity, "summary": summary, "count": n});
    if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
        o.extend(e);
    }
    v
}

impl SageStore {
    /// `readAudit(limit)`: the most recent audit events, newest first, as
    /// `SageAuditRecord`s (`sqliteRowToAuditRecord`).
    pub fn read_audit(&self, limit: Option<f64>) -> StoreResult<Vec<Value>> {
        let limit = limit.unwrap_or(50.0).floor().max(1.0) as i64;
        let mut st = self
            .conn
            .prepare("SELECT event, at, trace_id, data FROM audit_log ORDER BY id DESC LIMIT ?1")
            .map_err(sql_err)?;
        let rows = st
            .query_map([limit], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        Ok(rows
            .into_iter()
            .map(|(event, at, trace, data)| {
                let parsed = data
                    .and_then(|d| serde_json::from_str::<Value>(&d).ok())
                    .and_then(|v| match v {
                        Value::Object(m) => Some(m),
                        _ => None,
                    })
                    .unwrap_or_default();
                let mut rec = Map::new();
                rec.insert("schemaVersion".into(), json!(1));
                rec.insert("event".into(), json!(event));
                rec.insert("at".into(), json!(at));
                for key in ["memoryId", "source", "reason"] {
                    if let Some(s) = parsed.get(key).and_then(Value::as_str) {
                        rec.insert(key.into(), json!(s));
                    }
                }
                if let Some(t) = trace.filter(|t| !t.is_empty()) {
                    rec.insert("traceId".into(), json!(t));
                }
                if let Some(d) = parsed.get("details") {
                    rec.insert("details".into(), d.clone());
                }
                Value::Object(rec)
            })
            .collect())
    }

    /// `doctor({ fix, hygieneOptions })`: apply the named repairs (each once,
    /// in the given order), then measure metrics and run the checks.
    pub fn doctor(
        &mut self,
        fixes: &[String],
        hygiene: &HygieneOptions,
        ctx: &MutationCtx,
    ) -> StoreResult<Value> {
        let mut seen: Vec<&str> = Vec::new();
        for f in fixes {
            if !DOCTOR_FIX_IDS.contains(&f.as_str()) {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidRequest,
                    format!(
                        "unknown doctor fix: {f}. Known: {}",
                        DOCTOR_FIX_IDS.join(", ")
                    ),
                ));
            }
            if !seen.contains(&f.as_str()) {
                seen.push(f.as_str());
            }
        }
        if !seen.is_empty() {
            self.require_writable()?;
        }
        let mut applied = Vec::new();
        for f in seen {
            applied.push(self.apply_fix(f, hygiene, ctx)?);
        }
        let mut report = json!({
            "generatedAt": ctx.now_iso,
            "metrics": self.doctor_metrics()?,
            "checks": self.doctor_checks(&ctx.now_iso)?,
        });
        if !applied.is_empty() {
            report["fixesApplied"] = Value::Array(applied);
        }
        Ok(report)
    }

    fn doctor_metrics(&self) -> StoreResult<Value> {
        let c = &self.conn;
        let mut by_status = Map::new();
        let mut total = 0i64;
        {
            let mut st = c
                .prepare("SELECT status, COUNT(*) AS n FROM memories GROUP BY status")
                .map_err(sql_err)?;
            let rows = st
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            for (s, n) in rows {
                total += n;
                by_status.insert(s, json!(n));
            }
        }
        let (injections, uses): (Option<f64>, Option<f64>) = c
            .query_row(
                "SELECT SUM(COALESCE(json_extract(data, '$.injectionCount'), 0)),
                        SUM(COALESCE(json_extract(data, '$.useCount'), 0))
                   FROM memories WHERE status = 'active' AND json_valid(data)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql_err)?;
        let (injections, uses) = (injections.unwrap_or(0.0), uses.unwrap_or(0.0));
        let db = self.path().to_path_buf();
        let wal = std::path::PathBuf::from(format!("{}-wal", db.display()));
        let mut m = json!({
            "total": total,
            "active": by_status.get("active").cloned().unwrap_or(json!(0)),
            "byStatus": by_status,
            "edges": count(c, "SELECT COUNT(*) FROM edges", [])?,
            "pendingCandidates": count(c, "SELECT COUNT(*) FROM candidates WHERE status = 'pending'", [])?,
            "injections": num(injections),
            "uses": num(uses),
            "precision": if injections > 0.0 { num(uses / injections) } else { json!(0) },
            "injectedNeverUsed": count(
                c,
                "SELECT COUNT(*) FROM memories
                  WHERE status = 'active' AND json_valid(data) AND (audience IS NULL OR audience = '')
                    AND COALESCE(json_extract(data, '$.injectionCount'), 0) >= ?1
                    AND COALESCE(json_extract(data, '$.useCount'), 0) = 0",
                [NEVER_USED_REPORT_MIN_INJECTIONS],
            )?,
            "unanchoredActive": count(
                c,
                "SELECT COUNT(*) FROM memories
                  WHERE status = 'active' AND json_valid(data)
                    AND COALESCE(json_array_length(json_extract(data, '$.anchors')), 0) = 0",
                [],
            )?,
            "neverVerifiedActive": count(
                c,
                "SELECT COUNT(*) FROM memories
                  WHERE status = 'active' AND json_valid(data) AND json_extract(data, '$.lastVerifiedAt') IS NULL
                    AND COALESCE(json_array_length(json_extract(data, '$.anchors')), 0) > 0",
                [],
            )?,
        });
        if let Some(b) = file_bytes(&db) {
            m["dbBytes"] = json!(b);
        }
        if let Some(b) = file_bytes(&wal) {
            m["walBytes"] = json!(b);
        }
        Ok(m)
    }

    fn fts_present(&self) -> StoreResult<bool> {
        Ok(count(
            &self.conn,
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'memories_fts'",
            [],
        )? > 0)
    }

    fn fts_check(&self) -> StoreResult<Value> {
        let broken = |summary: String| json!({"id": "fts-integrity", "title": "Full-text index", "severity": "error", "summary": summary, "fix": "fts-rebuild"});
        if !self.fts_present()? {
            return Ok(
                json!({"id": "fts-integrity", "title": "Full-text index", "severity": "info",
                "summary": "FTS5 is unavailable; search uses the LIKE fallback"}),
            );
        }
        // Rank 0 checks the index's own structure only (see the baseline:
        // `memories` keeps its text inside JSON `data`).
        if let Err(e) = self.conn.execute(
            "INSERT INTO memories_fts(memories_fts, rank) VALUES('integrity-check', 0)",
            [],
        ) {
            return Ok(broken(format!("FTS index is corrupt: {e}")));
        }
        let indexed = count(&self.conn, "SELECT COUNT(*) FROM memories_fts_docsize", [])?;
        let rows = count(&self.conn, "SELECT COUNT(*) FROM memories", [])?;
        if indexed != rows {
            return Ok(broken(format!(
                "FTS index holds {indexed} documents for {rows} memories"
            )));
        }
        Ok(
            json!({"id": "fts-integrity", "title": "Full-text index", "severity": "ok",
            "summary": "FTS index is consistent"}),
        )
    }

    fn doctor_checks(&self, now: &str) -> StoreResult<Vec<Value>> {
        let c = &self.conn;
        let mut checks = Vec::new();

        let integrity: Vec<String> = strings(c, "PRAGMA quick_check", [])?
            .into_iter()
            .filter(|v| v != "ok")
            .collect();
        checks.push(check(
            "sqlite-integrity",
            "SQLite integrity",
            integrity.len() as i64,
            "error",
            "PRAGMA quick_check reported problems".into(),
            json!({"samples": integrity.iter().take(SAMPLE_LIMIT).collect::<Vec<_>>()}),
        ));

        checks.push(self.fts_check()?);

        let expired = count(
            c,
            "SELECT COUNT(*) FROM memories
              WHERE status = 'active' AND json_valid(data) AND json_extract(data, '$.expiresAt') IS NOT NULL
                AND json_extract(data, '$.expiresAt') <= ?1",
            [now],
        )?;
        let expired_samples = strings(
            c,
            &format!(
                "SELECT id FROM memories WHERE status = 'active' AND json_valid(data)
                   AND json_extract(data, '$.expiresAt') <= ?1 LIMIT {SAMPLE_LIMIT}"
            ),
            [now],
        )?;
        checks.push(check(
            "expired-active",
            "Expired memories still active",
            expired,
            "warn",
            format!(
                "{expired} active memories are past expiresAt — hygiene has not run since they expired"
            ),
            json!({"fix": "hygiene", "samples": expired_samples}),
        ));

        let hub_rows = hubs(c)?;
        let hub_edges: i64 = hub_rows.iter().map(|(_, n)| n).sum();
        checks.push(check(
            "graph-hubs",
            "Graph hub nodes",
            hub_rows.len() as i64,
            "warn",
            format!(
                "{} nodes carry more than {GRAPH_HUB_DEGREE} same_topic/related_to edges ({hub_edges} edges of graph noise)",
                hub_rows.len()
            ),
            json!({"fix": "graph-hubs", "samples": hub_rows.iter().take(SAMPLE_LIMIT)
                .map(|(v, n)| format!("{v} ({n})")).collect::<Vec<_>>()}),
        ));

        let dangling = count(
            c,
            &format!("SELECT COUNT(*) FROM edges WHERE {DANGLING_EDGE_FILTER}"),
            [],
        )?;
        checks.push(check(
            "dangling-edges",
            "Edges to missing or deleted memories",
            dangling,
            "warn",
            format!("{dangling} edges point at a memory that is gone or deleted"),
            json!({"fix": "dangling-edges"}),
        ));

        let mode = strings(c, RUNTIME_MODE_AUDIENCE_SQL, [RUNTIME_DEFAULT_MODE])?;
        checks.push(check(
            "audience-runtime-mode",
            "Audiences scoped to the runtime mode",
            mode.len() as i64,
            "warn",
            format!(
                "{} memories are scoped to mode \"default\": hidden from path injection, pushed into every default-mode subagent",
                mode.len()
            ),
            json!({"fix": "audience-runtime-mode", "samples": mode.iter().take(SAMPLE_LIMIT).collect::<Vec<_>>()}),
        ));

        const HEAVY: &str =
            "status = 'active' AND json_valid(data) AND (audience IS NULL OR audience = '')
            AND COALESCE(json_extract(data, '$.injectionCount'), 0) >= 100
            AND COALESCE(json_extract(data, '$.useCount'), 0) = 0";
        let never_used = strings(
            c,
            &format!(
                "SELECT id || ' (' || json_extract(data, '$.injectionCount') || 'x)' FROM memories
                  WHERE {HEAVY} ORDER BY json_extract(data, '$.injectionCount') DESC LIMIT {SAMPLE_LIMIT}"
            ),
            [],
        )?;
        let heavy = count(
            c,
            &format!("SELECT COUNT(*) FROM memories WHERE {HEAVY}"),
            [],
        )?;
        checks.push(check(
            "injected-never-used",
            "Heavily injected, never used",
            heavy,
            "info",
            format!(
                "{heavy} memories were injected 100+ times without a single recorded use — review them"
            ),
            json!({"samples": never_used}),
        ));

        let pending = count(
            c,
            "SELECT COUNT(*) FROM candidates WHERE status = 'pending'",
            [],
        )?;
        if pending > PENDING_CANDIDATE_WARN {
            let oldest: Option<String> = c
                .query_row(
                    "SELECT MIN(created_at) FROM candidates WHERE status = 'pending'",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?
                .flatten();
            checks.push(json!({
                "id": "candidate-backlog", "title": "Review backlog", "severity": "warn",
                "summary": format!("{pending} review candidates are pending (oldest {})", oldest.as_deref().unwrap_or("?")),
                "count": pending,
            }));
        } else {
            checks.push(check(
                "candidate-backlog",
                "Review backlog",
                pending,
                "info",
                format!("{pending} review candidates pending"),
                json!({}),
            ));
        }

        let wal = file_bytes(&std::path::PathBuf::from(format!(
            "{}-wal",
            self.path().display()
        )))
        .unwrap_or(0);
        checks.push(if wal > WAL_WARN_BYTES {
            json!({"id": "wal-size", "title": "WAL size", "severity": "info",
                "summary": format!("WAL is {} MB; it shrinks on the next checkpoint", (wal as f64 / 1024.0 / 1024.0).round() as u64)})
        } else {
            json!({"id": "wal-size", "title": "WAL size", "severity": "ok", "summary": "WAL size is normal"})
        });
        Ok(checks)
    }

    fn apply_fix(
        &mut self,
        fix: &str,
        hygiene: &HygieneOptions,
        ctx: &MutationCtx,
    ) -> StoreResult<Value> {
        let now = ctx.now_iso.clone();
        match fix {
            "hygiene" => {
                let r = self.hygiene(hygiene, ctx)?;
                Ok(json!({
                    "fix": fix,
                    "changed": r.deleted + r.staled + r.superseded + r.review_candidates_created,
                    "detail": format!(
                        "hygiene: {} expired removed, {} staled, {} superseded, {} review candidates",
                        r.deleted, r.staled, r.superseded, r.review_candidates_created
                    ),
                }))
            }
            "graph-hubs" => self.in_transaction(|s| {
                let hub_rows = hubs(&s.conn)?;
                let mut changed = 0usize;
                for (v, _) in &hub_rows {
                    changed += s
                        .conn
                        .execute(
                            "DELETE FROM edges WHERE relation IN (?1, ?2) AND (from_node = ?3 OR to_node = ?3)",
                            params![SOFT_RELATIONS[0], SOFT_RELATIONS[1], v],
                        )
                        .map_err(sql_err)?;
                }
                audit(
                    &s.conn,
                    "memory.doctor_fix",
                    &now,
                    json!({"fix": fix, "details": {"hubs": hub_rows.len(), "edges": changed}}),
                )
                .map_err(sql_err)?;
                Ok(json!({"fix": fix, "changed": changed,
                    "detail": format!("removed {changed} soft edges from {} hub nodes", hub_rows.len())}))
            }),
            "dangling-edges" => self.in_transaction(|s| {
                let changed = s
                    .conn
                    .execute(
                        &format!("DELETE FROM edges WHERE {DANGLING_EDGE_FILTER}"),
                        [],
                    )
                    .map_err(sql_err)?;
                audit(
                    &s.conn,
                    "memory.doctor_fix",
                    &now,
                    json!({"fix": fix, "details": {"edges": changed}}),
                )
                .map_err(sql_err)?;
                Ok(json!({"fix": fix, "changed": changed,
                    "detail": format!("removed {changed} edges to missing or deleted memories")}))
            }),
            "audience-runtime-mode" => self.in_transaction(|s| {
                let ids = strings(&s.conn, RUNTIME_MODE_AUDIENCE_SQL, [RUNTIME_DEFAULT_MODE])?;
                let (mut changed, mut skipped) = (0usize, 0usize);
                for id in ids {
                    let Some((mut raw, rec)) = read_raw(&s.conn, &id).map_err(sql_err)? else {
                        skipped += 1;
                        continue;
                    };
                    let Some(Value::Object(mut audience)) = raw.get("audience").cloned() else {
                        continue;
                    };
                    let modes: Vec<Value> = audience
                        .remove("modes")
                        .and_then(|m| m.as_array().cloned())
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|m| m.as_str() != Some(RUNTIME_DEFAULT_MODE))
                        .collect();
                    if !modes.is_empty() {
                        audience.insert("modes".into(), Value::Array(modes));
                    }
                    let empty = audience
                        .values()
                        .all(|v| v.as_array().is_none_or(|a| a.is_empty()));
                    if empty {
                        raw.remove("audience");
                    } else {
                        raw.insert("audience".into(), Value::Object(audience));
                    }
                    raw.insert("revision".into(), json!(rec.revision + 1));
                    raw.insert("updatedAt".into(), json!(now));
                    upsert_raw(&s.conn, &mut raw)?;
                    // Subagent-spawn injections can never be credited with a
                    // use; reset them (direct write: the upsert keeps the
                    // larger live counter).
                    s.conn
                        .execute(
                            "UPDATE memories SET data = json_set(data, '$.injectionCount', 0) WHERE id = ?1",
                            [&id],
                        )
                        .map_err(sql_err)?;
                    changed += 1;
                }
                audit(
                    &s.conn,
                    "memory.doctor_fix",
                    &now,
                    json!({"fix": fix, "details": {"memories": changed, "skippedCorrupt": skipped}}),
                )
                .map_err(sql_err)?;
                Ok(json!({"fix": fix, "changed": changed, "detail": if skipped > 0 {
                    format!("lifted runtime mode \"default\" from {changed} audiences, skipped {skipped} unreadable")
                } else {
                    format!("lifted runtime mode \"default\" from {changed} audiences")
                }}))
            }),
            "fts-rebuild" => self.in_transaction(|s| {
                if !s.fts_present()? {
                    return Ok(
                        json!({"fix": fix, "changed": 0, "detail": "FTS5 unavailable; nothing to rebuild"}),
                    );
                }
                s.conn
                    .execute_batch(
                        "INSERT INTO memories_fts(memories_fts) VALUES('delete-all');
                         INSERT INTO memories_fts(rowid, text, tags, audience)
                         SELECT rowid,
                           CASE WHEN json_valid(data) THEN COALESCE(json_extract(data, '$.text'), '') ELSE '' END,
                           COALESCE(tags, ''),
                           COALESCE(audience, '')
                         FROM memories;",
                    )
                    .map_err(sql_err)?;
                audit(&s.conn, "memory.doctor_fix", &now, json!({"fix": fix})).map_err(sql_err)?;
                Ok(json!({"fix": fix, "changed": 1, "detail": "rebuilt the full-text index"}))
            }),
            _ => unreachable!("validated above"),
        }
    }
}
