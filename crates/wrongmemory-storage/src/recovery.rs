//! Recovery and backfill (WM-505), ported from `sqlite-store-recovery.ts` at
//! the baseline commit.
//!
//! * `recover`: an active record is returned unchanged; a superseded record
//!   resolves to the active head of its `supersededBy` chain (depth 64,
//!   cycles refused); a tombstone already restored by backfill resolves to
//!   that successor's head; otherwise the tombstone is restored in place.
//! * `backfill_recoverable`: dry-run by default. Applying creates a NEW
//!   active successor (`mem_<ulid>`) that supersedes the tombstone; the
//!   tombstone itself is kept as history.

use std::collections::{BTreeMap, HashSet};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use wrongmemory_contracts::{ErrorBody, ErrorCode, SageRecord};

use crate::MutationCtx;
use crate::mutate::{read_raw, sync_edges, upsert_raw};
use crate::store::{SageStore, StoreResult, Viewer, audit, decode_memory, sql_err};

const CHAIN_DEPTH: usize = 64;

fn invalid(msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::InvalidRequest, msg)
}

fn not_found() -> ErrorBody {
    ErrorBody::new(ErrorCode::NotFound, "memory not found")
}

/// `SageBackfillFilter`. Empty fields mean "no filter".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_before: Option<String>,
    /// Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_text: Option<bool>,
    /// Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_provenance: Option<bool>,
}

/// `SageBackfillRecord`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillRecord {
    pub original_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_active_id: Option<String>,
    pub reason: String,
    pub kind: String,
    pub scope: String,
    pub text_preview: String,
    pub deleted_at: String,
    pub persistence: String,
}

/// `SageBackfillReport`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillReport {
    pub started_at: String,
    pub completed_at: String,
    pub dry_run: bool,
    pub examined: usize,
    pub recoverable: usize,
    pub recovered: usize,
    pub skipped: usize,
    pub skipped_records: Vec<BackfillRecord>,
    pub recoverable_records: Vec<BackfillRecord>,
    pub by_kind: BTreeMap<String, usize>,
    pub by_reason: BTreeMap<String, usize>,
}

fn record(m: &SageRecord, reason: &str, new_active_id: Option<String>) -> BackfillRecord {
    BackfillRecord {
        original_id: m.id.clone(),
        new_active_id,
        reason: reason.into(),
        kind: m.kind.clone(),
        scope: m.scope.clone(),
        // `text.slice(0, 200)`: UTF-16 code units in JS; chars here.
        text_preview: m.text.chars().take(200).collect(),
        deleted_at: m.updated_at.clone(),
        persistence: m.persistence.clone().unwrap_or_else(|| "long_lived".into()),
    }
}

/// Every decodable memory, `ORDER BY updated_at DESC` (`listMemories`).
pub(crate) fn all_memories(conn: &Connection) -> StoreResult<Vec<SageRecord>> {
    let rows: Vec<String> = conn
        .prepare("SELECT data FROM memories ORDER BY updated_at DESC")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()
        })
        .map_err(sql_err)?;
    Ok(rows.iter().filter_map(|d| decode_memory(d)).collect())
}

fn get(conn: &Connection, id: &str) -> StoreResult<Option<SageRecord>> {
    Ok(read_raw(conn, id).map_err(sql_err)?.map(|(_, r)| r))
}

/// `followSupersededHead`.
fn follow_head(conn: &Connection, start: SageRecord) -> StoreResult<Option<SageRecord>> {
    let mut current = start;
    let mut seen = HashSet::new();
    seen.insert(current.id.clone());
    for _ in 0..CHAIN_DEPTH {
        let Some(next_id) = current.superseded_by.clone() else {
            return Ok((current.status == "active").then_some(current));
        };
        if !seen.insert(next_id.clone()) {
            return Ok(None);
        }
        match get(conn, &next_id)? {
            Some(next) => current = next,
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// `activeSuccessorHead`: a live memory that supersedes `id` (backfill).
fn active_successor_head(
    conn: &Connection,
    all: &[SageRecord],
    id: &str,
) -> StoreResult<Option<SageRecord>> {
    for m in all {
        if m.status == "deleted"
            || !m
                .supersedes
                .as_ref()
                .is_some_and(|s| s.iter().any(|x| x == id))
        {
            continue;
        }
        if let Some(head) = follow_head(conn, m.clone())? {
            return Ok(Some(head));
        }
    }
    Ok(None)
}

/// `duplicateKey`: scope, owner session, trimmed/whitespace-collapsed/lowercased text.
fn duplicate_key(m: &SageRecord) -> String {
    let text = m
        .text
        .trim()
        .split(crate::text::is_js_space)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!(
        "{}\u{0}{}\u{0}{}",
        m.scope,
        m.owner_session_id.as_deref().unwrap_or(""),
        text
    )
}

impl SageStore {
    /// `recoverSage`. Visibility-bound: a record the caller cannot see is
    /// `NOT_FOUND`, like an absent one.
    pub fn recover(
        &mut self,
        id: &str,
        reason: Option<&str>,
        ctx: &MutationCtx,
    ) -> StoreResult<SageRecord> {
        self.require_writable()?;
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        if self.get(id, &viewer)?.is_none() {
            return Err(not_found());
        }
        let reason = reason.unwrap_or("Manually recovered via API.").to_string();
        let now = ctx.now_iso.clone();
        let principal = ctx.principal_id.clone();
        self.in_transaction(|store| {
            let conn = &store.conn;
            let memory = get(conn, id)?.ok_or_else(not_found)?;
            if memory.status == "active" {
                return Ok(memory);
            }
            if memory.status == "superseded" {
                let head = follow_head(conn, memory)?.ok_or_else(|| {
                    invalid(format!(
                        "SAGE \"{id}\" is superseded but its active chain head is unavailable."
                    ))
                })?;
                audit(
                    conn,
                    "memory.recover_noop",
                    &now,
                    json!({"memoryId": id, "reason": "Already superseded; returning active chain head.",
                           "details": {"activeId": head.id, "principal": principal}}),
                )
                .map_err(sql_err)?;
                return Ok(head);
            }
            if memory.status != "deleted" {
                return Err(invalid(format!(
                    "SAGE \"{id}\" has status \"{}\" and cannot be recovered.",
                    memory.status
                )));
            }
            let all = all_memories(conn)?;
            if let Some(head) = active_successor_head(conn, &all, id)? {
                audit(
                    conn,
                    "memory.recover_noop",
                    &now,
                    json!({"memoryId": id, "reason": "Already restored by backfill; returning its active successor.",
                           "details": {"activeId": head.id, "principal": principal}}),
                )
                .map_err(sql_err)?;
                return Ok(head);
            }
            let (mut m, fresh) = read_raw(conn, id).map_err(sql_err)?.ok_or_else(not_found)?;
            m.insert("revision".into(), json!(fresh.revision + 1));
            m.insert("status".into(), json!("active"));
            m.remove("staleReason");
            m.insert("updatedAt".into(), json!(now));
            let restored = upsert_raw(conn, &mut m)?;
            sync_edges(conn, &restored, &now)?;
            audit(
                conn,
                "memory.recovered",
                &now,
                json!({"memoryId": id, "reason": reason, "details": {
                    "persistence": restored.persistence.as_deref().unwrap_or("long_lived"),
                    "principal": principal,
                }}),
            )
            .map_err(sql_err)?;
            Ok(restored)
        })
    }

    /// `backfillRecoverable`. `dry_run` defaults to true at the API layer;
    /// pass `false` explicitly to apply.
    pub fn backfill_recoverable(
        &mut self,
        filter: &BackfillFilter,
        dry_run: bool,
        ctx: &MutationCtx,
    ) -> StoreResult<BackfillReport> {
        if !dry_run {
            self.require_writable()?;
        }
        let now = ctx.now_iso.clone();
        let principal = ctx.principal_id.clone();
        let filter = filter.clone();
        let run = move |conn: &Connection| -> StoreResult<BackfillReport> {
            let requested: Option<HashSet<&String>> =
                filter.ids.as_ref().map(|v| v.iter().collect());
            let require_text = filter.require_text != Some(false);
            let require_provenance = filter.require_provenance != Some(false);
            let all = all_memories(conn)?;
            let deleted: Vec<&SageRecord> = all.iter().filter(|m| m.status == "deleted").collect();
            let mut already = HashSet::new();
            let mut live = HashSet::new();
            for m in &all {
                if m.status == "deleted" {
                    continue;
                }
                for p in m.supersedes.iter().flatten() {
                    already.insert(p.clone());
                }
                if m.status == "active" || m.status == "stale" {
                    live.insert(duplicate_key(m));
                }
            }
            let mut report = BackfillReport {
                started_at: now.clone(),
                completed_at: now.clone(),
                dry_run,
                examined: deleted.len(),
                recoverable: 0,
                recovered: 0,
                skipped: 0,
                skipped_records: vec![],
                recoverable_records: vec![],
                by_kind: BTreeMap::new(),
                by_reason: BTreeMap::new(),
            };
            let skip = |r: &mut BackfillReport, m: &SageRecord, reason: &str| {
                r.skipped += 1;
                *r.by_reason.entry(reason.into()).or_default() += 1;
                r.skipped_records.push(record(m, reason, None));
            };
            for m in deleted {
                let reason = if requested.as_ref().is_some_and(|ids| !ids.contains(&m.id)) {
                    Some("filter_ids")
                } else if already.contains(&m.id) {
                    Some("already_recovered")
                } else if filter.kinds.as_ref().is_some_and(|k| !k.contains(&m.kind)) {
                    Some("filter_kinds")
                } else if filter
                    .scopes
                    .as_ref()
                    .is_some_and(|s| !s.contains(&m.scope))
                {
                    Some("filter_scopes")
                } else if filter
                    .updated_after
                    .as_ref()
                    .is_some_and(|a| m.updated_at < *a)
                {
                    Some("filter_updatedAfter")
                } else if filter
                    .updated_before
                    .as_ref()
                    .is_some_and(|b| m.updated_at > *b)
                {
                    Some("filter_updatedBefore")
                } else if require_text && m.text.trim().is_empty() {
                    Some("empty_text")
                } else if require_provenance && m.sources.is_empty() && m.anchors.is_empty() {
                    Some("no_provenance")
                } else if !live.insert(duplicate_key(m)) {
                    Some("duplicate_active")
                } else {
                    None
                };
                match reason {
                    Some(r) => skip(&mut report, m, r),
                    None => {
                        report.recoverable += 1;
                        *report.by_kind.entry(m.kind.clone()).or_default() += 1;
                        report
                            .recoverable_records
                            .push(record(m, "recoverable", None));
                    }
                }
            }
            if !dry_run {
                for i in 0..report.recoverable_records.len() {
                    let original_id = report.recoverable_records[i].original_id.clone();
                    let Some((raw, original)) = read_raw(conn, &original_id).map_err(sql_err)?
                    else {
                        report.recoverable_records[i].reason = "race_lost".into();
                        *report.by_reason.entry("race_lost".into()).or_default() += 1;
                        continue;
                    };
                    if original.status != "deleted" {
                        report.recoverable_records[i].reason = "race_lost".into();
                        *report.by_reason.entry("race_lost".into()).or_default() += 1;
                        continue;
                    }
                    let new_id = format!("mem_{}", ulid::Ulid::generate());
                    let mut m: Map<String, Value> = raw;
                    m.insert("id".into(), json!(new_id));
                    m.insert("revision".into(), json!(1));
                    m.insert("status".into(), json!("active"));
                    m.remove("staleReason");
                    let mut tags = original.tags.clone();
                    if !tags.iter().any(|t| t == "backfill:recovered") {
                        tags.push("backfill:recovered".into());
                    }
                    m.insert("tags".into(), json!(tags));
                    let mut sup = original.supersedes.clone().unwrap_or_default();
                    if !sup.contains(&original.id) {
                        sup.push(original.id.clone());
                    }
                    m.insert("supersedes".into(), json!(sup));
                    m.remove("supersededBy");
                    if original.context_policy.as_deref() == Some("never") {
                        m.insert("contextPolicy".into(), json!("never"));
                    } else {
                        m.remove("contextPolicy");
                    }
                    m.insert("injectionCount".into(), json!(0));
                    m.insert("useCount".into(), json!(0));
                    m.insert("createdAt".into(), json!(now));
                    m.insert("updatedAt".into(), json!(now));
                    m.remove("lastAccessedAt");
                    m.remove("lastVerifiedAt");
                    m.remove("lastUsedAt");
                    let created = upsert_raw(conn, &mut m)?;
                    sync_edges(conn, &created, &now)?;
                    conn.execute(
                        "INSERT INTO edges (from_node, to_node, relation, weight, created_at)
                         VALUES (?1, ?2, 'supersedes', 1, ?3)
                         ON CONFLICT(from_node, to_node, relation) DO UPDATE SET weight = MAX(weight, excluded.weight)",
                        params![format!("mem:{new_id}"), format!("mem:{}", original.id), now],
                    )
                    .map_err(sql_err)?;
                    report.recoverable_records[i].new_active_id = Some(new_id.clone());
                    report.recovered += 1;
                    audit(
                        conn,
                        "memory.backfilled",
                        &now,
                        json!({"memoryId": original.id, "reason": "backfillRecoverable",
                               "details": {"newActiveId": new_id, "principal": principal}}),
                    )
                    .map_err(sql_err)?;
                }
            }
            report.completed_at = now.clone();
            audit(
                conn,
                if dry_run {
                    "memory.backfill_dry_run"
                } else {
                    "memory.backfill_applied"
                },
                &now,
                json!({"details": {
                    "examined": report.examined, "recoverable": report.recoverable,
                    "recovered": report.recovered, "skipped": report.skipped,
                }}),
            )
            .map_err(sql_err)?;
            Ok(report)
        };
        if self.writable() {
            self.in_transaction(|store| run(&store.conn))
        } else {
            // Read-only handle: report only, without the audit row.
            Err(ErrorBody::new(
                ErrorCode::ReadOnly,
                "store is admitted read-only (no service ownership)",
            ))
        }
    }
}
