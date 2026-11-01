//! Native SAGE mutations (WM-502/503): remember with exact/near-duplicate
//! merge, update (content, lifecycle, feedback), delete tombstones, and
//! injection/use counters. Ported from `sqlite-store-remember.ts`,
//! `sqlite-store-update.ts`, `sqlite-store-delete.ts`,
//! `sqlite-store-counters.ts`, `shared/model-feedback.ts` and
//! `sqlite-store-graph-helpers.ts` at the baseline commit.
//!
//! Existing records are rewritten by patching their stored JSON object, so
//! unknown fields and JS `null` values survive (architecture §6). A key set
//! to "undefined" in the JS source is removed here.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use wrongmemory_contracts::{
    ErrorBody, ErrorCode, MAX_METADATA_ITEMS, MemoryAnchor, MemorySource, MutationOutcome,
    PERSISTENCE_CLASSES, RememberParams, SAGE_KINDS, SAGE_SCOPES, SAGE_STATUSES, SOURCE_TYPES,
    SageRecord,
};

use crate::MutationCtx;
use crate::rules;
use crate::similarity;
use crate::store::{
    SageStore, StoreResult, Viewer, anchor_node, audit, decode_memory, fingerprint, insert_memory,
    legacy_scope_for, sql_err, sync_anchor_edges,
};
use crate::text::{
    collapse_js_space, js_length, normalize_slashes, normalize_text_key, unicode_fold,
};

const MAX_FEEDBACK: usize = 8;
const MAX_RELATIONSHIP_EDGES_PER_MEMORY: usize = 256;

fn invalid(msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::InvalidRequest, msg)
}

fn not_found() -> ErrorBody {
    ErrorBody::new(ErrorCode::NotFound, "memory not found")
}

fn clamp01(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Read a memory's raw JSON object and its typed view. Corrupt rows are
/// absent for mutation purposes, like `readSqliteSageRow` callers.
pub(crate) fn read_raw(
    conn: &Connection,
    id: &str,
) -> rusqlite::Result<Option<(Map<String, Value>, SageRecord)>> {
    let data: Option<String> = conn
        .query_row("SELECT data FROM memories WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(data.and_then(|d| {
        let rec = decode_memory(&d)?;
        match serde_json::from_str::<Value>(&d).ok()? {
            Value::Object(m) => Some((m, rec)),
            _ => None,
        }
    }))
}

const LIVE_COUNTER_COUNT_KEYS: [&str; 2] = ["injectionCount", "useCount"];
const LIVE_COUNTER_STAMP_KEYS: [&str; 2] = ["lastUsedAt", "lastAccessedAt"];

fn valid_stamp(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| crate::text::parse_iso_ms(s).is_some())
        .map(str::to_string)
}

/// `mergeLiveCounterFields`: a counter bump committed after the caller read
/// the row must survive a whole-record rewrite.
fn merge_live_counters(previous: Option<&str>, next: &mut Map<String, Value>) {
    let Some(prev) = previous.and_then(|p| serde_json::from_str::<Value>(p).ok()) else {
        return;
    };
    let Some(prev) = prev.as_object() else { return };
    for key in LIVE_COUNTER_COUNT_KEYS {
        let p = prev
            .get(key)
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite())
            .unwrap_or(0.0);
        let n = next
            .get(key)
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite())
            .unwrap_or(0.0);
        if p > 0.0 || n > 0.0 {
            let m = p.max(n);
            next.insert(
                key.into(),
                if m.fract() == 0.0 {
                    json!(m as i64)
                } else {
                    json!(m)
                },
            );
        }
    }
    for key in LIVE_COUNTER_STAMP_KEYS {
        let p = valid_stamp(prev.get(key));
        let n = valid_stamp(next.get(key));
        match (p, n) {
            (None, Some(n)) => {
                next.insert(key.into(), json!(n));
            }
            (Some(p), None) => {
                next.insert(key.into(), json!(p));
            }
            (Some(p), Some(n)) => {
                next.insert(key.into(), json!(if p >= n { p } else { n }));
            }
            (None, None) => {}
        }
    }
}

/// `upsertMemory` + `upsertSqliteMemory` over a raw JSON object.
pub(crate) fn upsert_raw(conn: &Connection, m: &mut Map<String, Value>) -> StoreResult<SageRecord> {
    let id = m
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let previous: Option<String> = conn
        .query_row("SELECT data FROM memories WHERE id = ?1", [&id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(sql_err)?;
    merge_live_counters(previous.as_deref(), m);
    let data = Value::Object(m.clone()).to_string();
    let rec: SageRecord = serde_json::from_str(&data)
        .map_err(|e| ErrorBody::new(ErrorCode::Internal, format!("record shape: {e}")))?;
    let audience = m
        .get("audience")
        .filter(|v| !v.is_null())
        .map(Value::to_string);
    let tags = m
        .get("tags")
        .map(Value::to_string)
        .unwrap_or_else(|| "[]".into());
    conn.execute(
        "INSERT INTO memories
           (id, data, status, kind, scope, legacy_scope, importance, confidence, freshness,
            updated_at, created_at, audience, tags, owner_session_id, canonical_text, unicode_text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(id) DO UPDATE SET
           data = excluded.data, status = excluded.status, kind = excluded.kind,
           scope = excluded.scope, legacy_scope = excluded.legacy_scope,
           importance = excluded.importance, confidence = excluded.confidence,
           freshness = excluded.freshness, updated_at = excluded.updated_at,
           created_at = excluded.created_at, audience = excluded.audience,
           tags = excluded.tags, owner_session_id = excluded.owner_session_id,
           canonical_text = excluded.canonical_text",
        params![
            rec.id,
            data,
            rec.status,
            rec.kind,
            rec.scope,
            rec.legacy_scope
                .as_deref()
                .unwrap_or(legacy_scope_for(&rec.scope)),
            rec.importance,
            rec.confidence,
            rec.freshness,
            rec.updated_at,
            rec.created_at,
            audience,
            tags,
            rec.owner_session_id,
            normalize_text_key(&rec.text),
            unicode_fold(&rec.text),
        ],
    )
    .map_err(sql_err)?;
    Ok(rec)
}

/// `syncSqliteRelationshipEdges`: insert-only supersedes/contradicts edges.
fn sync_relationship_edges(conn: &Connection, rec: &SageRecord, now: &str) -> rusqlite::Result<()> {
    let from = format!("mem:{}", rec.id);
    let mut pairs: Vec<(&str, &String)> = Vec::new();
    for t in rec.supersedes.iter().flatten() {
        pairs.push(("supersedes", t));
    }
    for t in rec.contradicts.iter().flatten() {
        pairs.push(("contradicts", t));
    }
    let mut seen = HashSet::new();
    let mut written = 0;
    for (relation, target) in pairs {
        if written >= MAX_RELATIONSHIP_EDGES_PER_MEMORY {
            break;
        }
        let t = target.trim();
        if t.is_empty() {
            continue;
        }
        let to = format!("mem:{t}");
        if to == from || !seen.insert(format!("{to}\0{relation}")) {
            continue;
        }
        conn.execute(
            "INSERT INTO edges (from_node, to_node, relation, weight, created_at)
             VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(from_node, to_node, relation) DO UPDATE SET weight = MAX(weight, excluded.weight)",
            params![from, to, relation, now],
        )?;
        written += 1;
    }
    Ok(())
}

/// `syncAnchorEdges` host hook: anchors, then relationship assertions.
pub(crate) fn sync_edges(conn: &Connection, rec: &SageRecord, now: &str) -> StoreResult<()> {
    sync_anchor_edges(conn, rec, now).map_err(sql_err)?;
    sync_relationship_edges(conn, rec, now).map_err(sql_err)
}

pub(crate) fn cascade_delete_edges(conn: &Connection, node: &str) -> StoreResult<usize> {
    conn.execute(
        "DELETE FROM edges WHERE (from_node = ?1 OR to_node = ?1) AND relation != 'related_to'",
        [node],
    )
    .map_err(sql_err)
}

/// `cleanReferencingMemoriesBatch`: drop references to retired ids.
pub(crate) fn clean_referencing(
    conn: &Connection,
    targets: &HashSet<String>,
    now: &str,
) -> StoreResult<()> {
    if targets.is_empty() {
        return Ok(());
    }
    let rows: Vec<(String, String)> = conn
        .prepare(
            "SELECT id, data FROM memories
              WHERE status != 'deleted'
                AND CASE WHEN json_valid(data) THEN (
                  json_extract(data, '$.supersededBy') IS NOT NULL
                  OR COALESCE(json_array_length(json_extract(data, '$.supersedes')), 0) > 0
                  OR COALESCE(json_array_length(json_extract(data, '$.contradicts')), 0) > 0
                ) ELSE 0 END",
        )
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()
        })
        .map_err(sql_err)?;
    for (id, data) in rows {
        if targets.contains(&id) || decode_memory(&data).is_none() {
            continue;
        }
        let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let mut changed = false;
        for key in ["supersedes", "contradicts"] {
            if let Some(Value::Array(a)) = m.get(key)
                && a.iter()
                    .any(|v| v.as_str().is_some_and(|s| targets.contains(s)))
            {
                let kept: Vec<Value> = a
                    .iter()
                    .filter(|v| !v.as_str().is_some_and(|s| targets.contains(s)))
                    .cloned()
                    .collect();
                m.insert(key.into(), Value::Array(kept));
                changed = true;
            }
        }
        if m.get("supersededBy")
            .and_then(Value::as_str)
            .is_some_and(|s| targets.contains(s))
        {
            m.remove("supersededBy");
            changed = true;
        }
        if changed {
            bump(&mut m, now);
            upsert_raw(conn, &mut m)?;
        }
    }
    Ok(())
}

fn bump(m: &mut Map<String, Value>, now: &str) {
    let rev = m.get("revision").and_then(Value::as_i64).unwrap_or(0);
    m.insert("revision".into(), json!(rev + 1));
    m.insert("updatedAt".into(), json!(now));
}

/// `normalizeTags`: strip one leading `#`, trim, lowercase, dedupe, drop empty.
pub(crate) fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.strip_prefix('#').unwrap_or(t).trim().to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// `normalizeSources` with `dedupeSources`.
pub(crate) fn normalize_sources(sources: &[MemorySource]) -> Vec<MemorySource> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for s in sources {
        let mut s = s.clone();
        s.path = s.path.as_deref().map(|p| normalize_slashes(p.trim()));
        s.command = s
            .command
            .as_deref()
            .map(|c| collapse_js_space(c.trim()))
            .filter(|c| !c.is_empty());
        let key = json!([
            s.kind,
            s.session_id,
            s.extra.get("toolUseId"),
            s.path,
            s.command,
            s.extra.get("excerptHash")
        ])
        .to_string();
        if seen.insert(key) {
            out.push(s);
        }
    }
    out
}

impl SageStore {
    /// `normalizeAnchors` with `dedupeAnchors`.
    pub(crate) fn normalize_anchors_full(&self, anchors: &[MemoryAnchor]) -> Vec<MemoryAnchor> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for a in anchors {
            let mut a = a.clone();
            a.path = a.path.as_deref().map(|p| self.relative_path(p));
            a.symbol = a
                .symbol
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            a.command = a
                .command
                .as_deref()
                .map(|c| collapse_js_space(c.trim()))
                .filter(|c| !c.is_empty());
            a.role = a
                .role
                .as_deref()
                .map(|r| r.trim().to_lowercase())
                .filter(|r| !r.is_empty());
            let key = json!([
                a.kind,
                a.path,
                a.symbol,
                a.command,
                a.role,
                a.extra.get("contentHash"),
                a.extra.get("gitBlobHash"),
                a.extra.get("lineStart"),
                a.extra.get("lineEnd")
            ])
            .to_string();
            if seen.insert(key) {
                out.push(a);
            }
        }
        out
    }

    /// Full `rememberSage`: validation, quality caps, exact and near
    /// duplicate merge, relationship edges, receipt.
    pub fn remember_full(
        &mut self,
        p: &RememberParams,
        ctx: &MutationCtx,
    ) -> StoreResult<MutationOutcome> {
        self.require_writable()?;
        let input = to_value(p);
        rules::reject_if_unsafe(&input)?;
        let scope = p.scope.clone().unwrap_or_else(|| "project".into());
        let kind = p.kind.clone().unwrap_or_else(|| "fact".into());
        let text = rules::validate_text(&p.text)?;
        if !SAGE_SCOPES.contains(&scope.as_str()) {
            return Err(invalid("Invalid SAGE scope."));
        }
        if !SAGE_KINDS.contains(&kind.as_str()) {
            return Err(invalid("Invalid SAGE kind."));
        }
        let owner = if scope == "session" {
            Some(ctx.session_id.clone().ok_or_else(|| {
                invalid("SAGE scope 'session' requires ownerSessionId so the memory can be isolated to its owning session.")
            })?)
        } else {
            None
        };
        for (name, n) in [
            ("tags", p.tags.len()),
            ("anchors", p.anchors.len()),
            ("sources", p.sources.len()),
            ("supersedes", p.supersedes.len()),
            ("contradicts", p.contradicts.len()),
        ] {
            if n > MAX_METADATA_ITEMS {
                return Err(invalid(format!(
                    "SAGE {name} exceeds {MAX_METADATA_ITEMS} items."
                )));
            }
        }
        rules::validate_tags(&p.tags)?;
        if let Some(per) = &p.persistence
            && !PERSISTENCE_CLASSES.contains(&per.as_str())
        {
            return Err(invalid(format!(
                "Invalid SAGE persistence: expected one of permanent, long_lived, short_lived, got \"{per}\"."
            )));
        }
        if let Some(e) = &p.expires_at
            && crate::text::parse_iso_ms(e).is_none()
        {
            return Err(invalid(
                "SAGE expiresAt must be a valid ISO-8601 timestamp.",
            ));
        }
        if rules::STRUCTURAL_KINDS.contains(&kind.as_str()) && p.anchors.is_empty() {
            return Err(invalid(format!(
                "SAGE kind \"{kind}\" requires at least one anchor (file/symbol/command binding)."
            )));
        }
        if scope != "session" && rules::is_ephemeral(&text) {
            return Err(rules::ephemeral_rejection());
        }
        let audience = match &p.audience {
            Some(a) => rules::normalize_audience(&to_value(a))?,
            None => None,
        };
        rules::validate_anchors(&p.anchors)?;
        if p.sources
            .iter()
            .any(|s| !SOURCE_TYPES.contains(&s.kind.as_str()))
        {
            return Err(invalid("Invalid SAGE source type."));
        }
        let validity = match &p.validity {
            Some(v) => rules::normalize_validity(&to_value(v))?,
            None => None,
        };
        let tags = normalize_tags(&p.tags);
        let anchors = self.normalize_anchors_full(&p.anchors);
        let default_source = [MemorySource {
            kind: "user".into(),
            ..Default::default()
        }];
        let sources = normalize_sources(if p.sources.is_empty() {
            &default_source
        } else {
            &p.sources
        });
        let caps = rules::quality_caps(&text, &kind, &scope, anchors.len());
        let importance = match p.importance {
            Some(v) => clamp01(v),
            None => clamp01(0.6).min(caps.importance),
        };
        let confidence = match p.confidence {
            Some(v) => clamp01(v),
            None => clamp01(0.8).min(caps.confidence),
        };
        let freshness = clamp01(p.freshness.unwrap_or(1.0));
        let canonical = normalize_text_key(&text);
        let audience_key = audience.as_ref().map(Value::to_string);
        let validity_key = validity
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_else(|| "null".into());
        let fp = fingerprint(&input);
        let now = ctx.now_iso.clone();
        let project_root_anchors = anchors.clone();
        self.with_receipt(ctx, "memory.remember", &fp, |conn| {
            // Exact duplicate: same scope/canonical text/audience/session and validity.
            let mut sql = String::from(
                "SELECT data FROM memories WHERE status IN ('active','stale') AND scope = ?1
                   AND canonical_text = ?2 AND audience IS ?3",
            );
            if owner.is_some() {
                sql.push_str(" AND owner_session_id = ?4");
            }
            let exact_rows: Vec<String> = conn
                .prepare(&sql)
                .and_then(|mut s| {
                    if let Some(o) = &owner {
                        s.query_map(params![scope, canonical, audience_key, o], |r| r.get(0))?
                            .collect::<rusqlite::Result<Vec<String>>>()
                    } else {
                        s.query_map(params![scope, canonical, audience_key], |r| r.get(0))?
                            .collect::<rusqlite::Result<Vec<String>>>()
                    }
                })
                .map_err(sql_err)?;
            let vkey = |r: &SageRecord| {
                r.validity
                    .as_ref()
                    .map(|v| to_value(v).to_string())
                    .unwrap_or_else(|| "null".into())
            };
            let exact = exact_rows
                .iter()
                .filter_map(|d| decode_memory(d))
                .find(|r| vkey(r) == validity_key);
            let is_exact = exact.is_some();
            let existing = match exact {
                Some(e) => Some(e),
                None => find_near_duplicate(
                    conn,
                    &scope,
                    audience_key.as_deref(),
                    &kind,
                    &text,
                    &project_root_anchors,
                    &validity_key,
                    owner.as_deref(),
                )?,
            };
            if let Some(existing) = existing {
                let (mut m, old) = read_raw(conn, &existing.id)
                    .map_err(sql_err)?
                    .ok_or_else(not_found)?;
                let prefer_incoming = !is_exact
                    && similarity::tokenize_count(&text) > similarity::tokenize_count(&old.text)
                    && js_length(&text) > js_length(&old.text);
                let reactivated = old.status == "stale";
                if reactivated {
                    m.insert("status".into(), json!("active"));
                    m.remove("staleReason");
                }
                if prefer_incoming {
                    m.insert("text".into(), json!(text));
                }
                let mut merged_tags = old.tags.clone();
                for t in &tags {
                    if !merged_tags.contains(t) {
                        merged_tags.push(t.clone());
                    }
                }
                m.insert("tags".into(), json!(merged_tags));
                let mut anchor_keys = HashSet::new();
                let mut merged_anchors = Vec::new();
                for a in old.anchors.iter().chain(anchors.iter()) {
                    let v = to_value(a);
                    if anchor_keys.insert(sorted_key(&v)) {
                        merged_anchors.push(v);
                    }
                }
                m.insert("anchors".into(), Value::Array(merged_anchors));
                if let Some(a) = &audience {
                    m.insert("audience".into(), a.clone());
                }
                let mut src_keys = HashSet::new();
                let mut merged_sources = Vec::new();
                for s in old.sources.iter().chain(sources.iter()) {
                    let v = to_value(s);
                    if src_keys.insert(v.to_string()) {
                        merged_sources.push(v);
                    }
                }
                m.insert("sources".into(), Value::Array(merged_sources));
                let union = |a: &Option<Vec<String>>, b: &[String]| {
                    let mut out: Vec<String> = a.clone().unwrap_or_default();
                    for x in b {
                        if !out.contains(x) {
                            out.push(x.clone());
                        }
                    }
                    out
                };
                let sup = union(&old.supersedes, &p.supersedes);
                let con = union(&old.contradicts, &p.contradicts);
                let grew = sup.len() > old.supersedes.as_ref().map_or(0, Vec::len)
                    || con.len() > old.contradicts.as_ref().map_or(0, Vec::len);
                m.insert("supersedes".into(), json!(sup));
                m.insert("contradicts".into(), json!(con));
                m.insert("importance".into(), json!(old.importance.max(importance)));
                m.insert("confidence".into(), json!(old.confidence.max(confidence)));
                m.insert("freshness".into(), json!(old.freshness.max(freshness)));
                if let Some(per) = &p.persistence {
                    let keep_permanent =
                        old.persistence.as_deref() == Some("permanent") && per != "permanent";
                    m.insert(
                        "persistence".into(),
                        json!(if keep_permanent {
                            "permanent"
                        } else {
                            per.as_str()
                        }),
                    );
                }
                if let Some(e) = &p.expires_at {
                    m.insert("expiresAt".into(), json!(e));
                }
                bump(&mut m, &now);
                let merged = upsert_raw(conn, &mut m)?;
                if anchors_changed(&merged.anchors, &old.anchors)
                    || merged.confidence != old.confidence
                    || merged.text != old.text
                    || grew
                {
                    sync_edges(conn, &merged, &now)?;
                }
                return Ok(MutationOutcome {
                    outcome: "memory-merged".into(),
                    id: merged.id.clone(),
                    revision: Some(merged.revision),
                    replayed: false,
                    record: to_value(&merged),
                });
            }
            let mut extra = Map::new();
            extra.insert(
                "wrongmemory".into(),
                json!({"principalId": ctx.principal_id, "client": ctx.client_label}),
            );
            let rec = SageRecord {
                id: ulid::Ulid::generate().to_string(),
                revision: 1,
                scope: scope.clone(),
                legacy_scope: None,
                kind: kind.clone(),
                status: "active".into(),
                context_policy: None,
                persistence: p.persistence.clone(),
                text: text.clone(),
                summary: None,
                importance,
                confidence,
                freshness,
                tags: tags.clone(),
                anchors: anchors.clone(),
                audience: audience
                    .as_ref()
                    .and_then(|a| serde_json::from_value(a.clone()).ok()),
                sources: sources.clone(),
                validity: validity
                    .as_ref()
                    .and_then(|v| serde_json::from_value(v.clone()).ok()),
                feedback: None,
                supersedes: (!p.supersedes.is_empty()).then(|| p.supersedes.clone()),
                superseded_by: None,
                contradicts: (!p.contradicts.is_empty()).then(|| p.contradicts.clone()),
                created_at: now.clone(),
                updated_at: now.clone(),
                last_accessed_at: None,
                last_verified_at: None,
                stale_reason: None,
                last_used_at: None,
                expires_at: p.expires_at.clone(),
                injection_count: None,
                use_count: None,
                owner_session_id: owner.clone(),
                extra,
            };
            insert_memory(conn, &rec).map_err(sql_err)?;
            sync_edges(conn, &rec, &now)?;
            Ok(MutationOutcome {
                outcome: "memory-committed".into(),
                id: rec.id.clone(),
                revision: Some(1),
                replayed: false,
                record: to_value(&rec),
            })
        })
    }

    /// `updateSage`. `delete_authorized` is the server-side grant decision
    /// (curator); the client's `force` flag alone never authorizes deletion.
    pub fn update(
        &mut self,
        id: &str,
        input: &Map<String, Value>,
        ctx: &MutationCtx,
        delete_authorized: bool,
    ) -> StoreResult<MutationOutcome> {
        self.require_writable()?;
        const ALLOWED: &[&str] = &[
            "text",
            "persistence",
            "kind",
            "status",
            "tags",
            "anchors",
            "importance",
            "confidence",
            "freshness",
            "audience",
            "supersedes",
            "contradicts",
            "supersededBy",
            "validity",
            "sources",
            "expectedRevision",
            "feedback",
            "force",
        ];
        if let Some(k) = input.keys().find(|k| !ALLOWED.contains(&k.as_str())) {
            return Err(invalid(if k == "scope" || k == "ownerSessionId" {
                format!("SAGE {k} is immutable through ordinary update.")
            } else {
                format!("unknown update field {k}")
            }));
        }
        rules::reject_if_unsafe(&Value::Object(input.clone()))?;
        let get_str = |k: &str| -> StoreResult<Option<String>> {
            match input.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                _ => Err(invalid(format!("{k} must be a string"))),
            }
        };
        let get_vec = |k: &str| -> StoreResult<Option<Vec<String>>> {
            match input.get(k) {
                None => Ok(None),
                Some(v) => serde_json::from_value(v.clone())
                    .map(Some)
                    .map_err(|_| invalid(format!("{k} must be an array of strings"))),
            }
        };
        let get_num = |k: &str| -> StoreResult<Option<f64>> {
            match input.get(k) {
                None => Ok(None),
                Some(v) => v
                    .as_f64()
                    .map(Some)
                    .ok_or_else(|| invalid(format!("{k} must be a number"))),
            }
        };
        let text = match get_str("text")? {
            Some(t) => Some(rules::validate_text(&t)?),
            None => None,
        };
        let persistence = get_str("persistence")?;
        if let Some(p) = &persistence
            && !PERSISTENCE_CLASSES.contains(&p.as_str())
        {
            return Err(invalid(format!(
                "SAGE persistence must be one of: permanent, long_lived, short_lived; got \"{p}\"."
            )));
        }
        let kind = get_str("kind")?;
        if let Some(k) = &kind
            && !SAGE_KINDS.contains(&k.as_str())
        {
            return Err(invalid(format!(
                "SAGE kind must be one of the known kinds; got \"{k}\"."
            )));
        }
        let status = get_str("status")?;
        if let Some(s) = &status
            && !SAGE_STATUSES.contains(&s.as_str())
        {
            return Err(invalid(format!("Invalid SAGE status: \"{s}\".")));
        }
        let tags = get_vec("tags")?;
        let supersedes = get_vec("supersedes")?;
        let contradicts = get_vec("contradicts")?;
        let anchors: Option<Vec<MemoryAnchor>> = match input.get("anchors") {
            None => None,
            Some(v) => Some(
                serde_json::from_value(v.clone())
                    .map_err(|_| invalid("SAGE anchors must be an array."))?,
            ),
        };
        for (name, n) in [
            ("tags", tags.as_ref().map_or(0, Vec::len)),
            ("anchors", anchors.as_ref().map_or(0, Vec::len)),
            ("supersedes", supersedes.as_ref().map_or(0, Vec::len)),
            ("contradicts", contradicts.as_ref().map_or(0, Vec::len)),
        ] {
            if n > MAX_METADATA_ITEMS {
                return Err(invalid(format!(
                    "SAGE {name} exceeds {MAX_METADATA_ITEMS} items."
                )));
            }
        }
        if let Some(t) = &tags {
            rules::validate_tags(t)?;
        }
        if let Some(a) = &anchors {
            rules::validate_anchors(a)?;
        }
        let expected = match input.get("expectedRevision") {
            None => None,
            Some(v) => Some(v.as_i64().ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::RevisionConflict,
                    "SAGE revision changed; read the current memory before updating it.",
                )
            })?),
        };
        let force = input.get("force").and_then(Value::as_bool).unwrap_or(false);
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        // Visibility first: hidden and absent are the same NOT_FOUND.
        if self.get(id, &viewer)?.is_none() {
            return Err(not_found());
        }
        let fp = fingerprint(&json!({"id": id, "input": input}));
        let now = ctx.now_iso.clone();
        let action = if input.contains_key("feedback") {
            "memory.feedback"
        } else {
            "memory.update"
        };
        let root_anchors = anchors.as_ref().map(|a| self.normalize_anchors_full(a));
        self.with_receipt(ctx, action, &fp, |conn| {
            let (mut m, existing) = read_raw(conn, id).map_err(sql_err)?.ok_or_else(not_found)?;
            if let Some(e) = expected
                && e != existing.revision
            {
                return Err(ErrorBody::new(
                    ErrorCode::RevisionConflict,
                    "SAGE revision changed; read the current memory before updating it.",
                ));
            }
            if let Some(fb) = input.get("feedback") {
                if input.iter().any(|(k, v)| k != "feedback" && k != "expectedRevision" && !v.is_null()) {
                    return Err(invalid("Submit SAGE feedback separately from content or lifecycle updates."));
                }
                return append_feedback(conn, m, &existing, fb, ctx, &now);
            }
            if let Some(srcs) = input.get("sources") {
                let s: Vec<MemorySource> = serde_json::from_value(srcs.clone())
                    .map_err(|_| invalid("Invalid SAGE source type."))?;
                if s.len() > MAX_METADATA_ITEMS {
                    return Err(invalid(format!("SAGE sources exceeds {MAX_METADATA_ITEMS} items.")));
                }
                if s.iter().any(|x| !SOURCE_TYPES.contains(&x.kind.as_str())) {
                    return Err(invalid("Invalid SAGE source type."));
                }
                m.insert("sources".into(), to_value(&normalize_sources(&s)));
            }
            if let Some(t) = &text
                && existing.scope != "session"
                && rules::is_ephemeral(t)
            {
                return Err(rules::ephemeral_rejection());
            }
            let superseded_by = get_str("supersededBy")?;
            if let Some(succ) = &superseded_by {
                if status.as_deref().unwrap_or(&existing.status) != "superseded" {
                    return Err(invalid("SAGE supersededBy requires status \"superseded\"."));
                }
                if succ == id {
                    return Err(invalid("SAGE memory cannot supersede itself."));
                }
                match read_raw(conn, succ).map_err(sql_err)? {
                    Some((_, s)) if s.status != "deleted" => {
                        if s.superseded_by.as_deref() == Some(id) {
                            return Err(invalid(format!(
                                "SAGE successor \"{succ}\" is itself superseded by \"{id}\"."
                            )));
                        }
                    }
                    _ => return Err(invalid(format!("SAGE successor \"{succ}\" not found."))),
                }
            }
            let resulting_kind = kind.clone().unwrap_or_else(|| existing.kind.clone());
            let resulting_anchor_count = root_anchors.as_ref().map_or(existing.anchors.len(), Vec::len);
            if (kind.is_some() || anchors.is_some())
                && rules::STRUCTURAL_KINDS.contains(&resulting_kind.as_str())
                && resulting_anchor_count == 0
            {
                return Err(invalid(format!(
                    "SAGE kind \"{resulting_kind}\" requires at least one anchor (file/symbol/command binding)."
                )));
            }
            if status.as_deref() == Some("deleted") {
                let permanent = existing.persistence.as_deref() == Some("permanent")
                    || persistence.as_deref() == Some("permanent");
                if !delete_authorized {
                    return Err(ErrorBody::new(
                        ErrorCode::Forbidden,
                        format!("SAGE \"{id}\" cannot be deleted without an explicit destructive grant."),
                    ));
                }
                if permanent && !force {
                    return Err(ErrorBody::new(
                        ErrorCode::Forbidden,
                        format!("SAGE \"{id}\" is marked 'permanent'; pass force=true to override (audited)."),
                    ));
                }
            }
            if let Some(v) = input.get("validity") {
                if v.is_null() {
                    m.remove("validity");
                } else {
                    match rules::normalize_validity(v)? {
                        Some(n) => m.insert("validity".into(), n),
                        None => m.remove("validity"),
                    };
                }
            }
            if let Some(t) = &text {
                m.insert("text".into(), json!(t));
            }
            if let Some(p) = &persistence {
                m.insert("persistence".into(), json!(p));
            }
            if let Some(k) = &kind {
                m.insert("kind".into(), json!(k));
            }
            if let Some(s) = &status
                && *s != existing.status
            {
                m.insert("status".into(), json!(s));
                if s == "stale" {
                    m.insert("staleReason".into(), json!("manual"));
                } else {
                    m.remove("staleReason");
                }
            }
            if let Some(t) = &tags {
                m.insert("tags".into(), json!(normalize_tags(t)));
            }
            if let Some(a) = &root_anchors {
                m.insert("anchors".into(), to_value(a));
            }
            for (k, v) in [
                ("importance", get_num("importance")?),
                ("confidence", get_num("confidence")?),
                ("freshness", get_num("freshness")?),
            ] {
                if let Some(v) = v {
                    m.insert(k.into(), json!(clamp01(v)));
                }
            }
            if let Some(a) = input.get("audience") {
                match rules::normalize_audience(a)? {
                    Some(n) => m.insert("audience".into(), n),
                    None => m.remove("audience"),
                };
            }
            if let Some(s) = &supersedes {
                m.insert("supersedes".into(), json!(s));
            }
            if let Some(c) = &contradicts {
                m.insert("contradicts".into(), json!(c));
            }
            if status.is_some() && status.as_deref() != Some("superseded") && existing.superseded_by.is_some() {
                m.remove("supersededBy");
            }
            if let Some(s) = &superseded_by {
                m.insert("supersededBy".into(), json!(s));
            }
            if status.as_deref() == Some("deleted") {
                m.remove("supersedes");
                m.remove("contradicts");
                m.remove("supersededBy");
            }
            m.insert("revision".into(), json!(existing.revision + 1));
            m.insert("updatedAt".into(), json!(now));
            let updated = upsert_raw(conn, &mut m)?;
            let status_affects = status.is_some() && existing.status != updated.status && updated.status != "deleted";
            if updated.status != "deleted"
                && (anchors.is_some()
                    || input.contains_key("confidence")
                    || supersedes.is_some()
                    || contradicts.is_some()
                    || status_affects)
            {
                sync_edges(conn, &updated, &now)?;
            }
            if existing.status != "deleted" && updated.status == "deleted" {
                tombstone_cleanup(conn, id, &now, force, &updated, ctx)?;
            }
            Ok(MutationOutcome {
                outcome: "memory-committed".into(),
                id: id.to_string(),
                revision: Some(updated.revision),
                replayed: false,
                record: to_value(&updated),
            })
        })
    }

    /// `deleteSage`: force/grant-gated tombstone (never a physical removal).
    pub fn delete(
        &mut self,
        id: &str,
        reason: &str,
        force: bool,
        never_inject: bool,
        ctx: &MutationCtx,
        delete_authorized: bool,
    ) -> StoreResult<MutationOutcome> {
        self.require_writable()?;
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        if self.get(id, &viewer)?.is_none() {
            return Err(not_found());
        }
        let fp = fingerprint(
            &json!({"id": id, "reason": reason, "force": force, "neverInject": never_inject}),
        );
        let now = ctx.now_iso.clone();
        self.with_receipt(ctx, "memory.delete", &fp, |conn| {
            let (mut m, fresh) = read_raw(conn, id).map_err(sql_err)?.ok_or_else(not_found)?;
            if fresh.status == "deleted" {
                return Ok(MutationOutcome {
                    outcome: "memory-committed".into(),
                    id: id.into(),
                    revision: Some(fresh.revision),
                    replayed: false,
                    record: to_value(&fresh),
                });
            }
            if !delete_authorized || !force {
                return Err(ErrorBody::new(
                    ErrorCode::Forbidden,
                    if fresh.persistence.as_deref() == Some("permanent") {
                        format!("SAGE \"{id}\" is marked 'permanent' and cannot be deleted. Pass force=true to override; the override is audited.")
                    } else {
                        format!("SAGE \"{id}\" cannot be deleted without explicit authorization (destructive grant and force=true).")
                    },
                ));
            }
            let mut targets = HashSet::new();
            targets.insert(id.to_string());
            clean_referencing(conn, &targets, &now)?;
            // Re-read: cleanup never touches the target itself, but keep the
            // freshest counters in the tombstone.
            if let Some((fresh_m, _)) = read_raw(conn, id).map_err(sql_err)? {
                m = fresh_m;
            }
            m.insert("status".into(), json!("deleted"));
            m.insert("revision".into(), json!(fresh.revision + 1));
            m.insert("updatedAt".into(), json!(now));
            if never_inject {
                m.insert("contextPolicy".into(), json!("never"));
            }
            m.remove("supersedes");
            m.remove("contradicts");
            m.remove("supersededBy");
            let tomb = upsert_raw(conn, &mut m)?;
            let node = format!("mem:{id}");
            let removed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM edges WHERE (from_node = ?1 OR to_node = ?1) AND relation != 'related_to'",
                    [&node],
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            cascade_delete_edges(conn, &node)?;
            audit(
                conn,
                "memory.deleted",
                &now,
                json!({"memoryId": id, "reason": reason, "details": {
                    "removedEdges": removed, "force": force,
                    "contextPolicy": if never_inject { "never" } else { "eligible" },
                    "principal": ctx.principal_id,
                }}),
            )
            .map_err(sql_err)?;
            Ok(MutationOutcome {
                outcome: "memory-committed".into(),
                id: id.into(),
                revision: Some(tomb.revision),
                replayed: false,
                record: to_value(&tomb),
            })
        })
    }

    /// `recordInjection` / `recordUse`: telemetry only; never advances
    /// `updatedAt`, revision, FTS, or the HQ projection.
    pub fn record_usage(
        &mut self,
        ids: &[String],
        kind: UsageKind,
        label: &str,
        ctx: &MutationCtx,
    ) -> StoreResult<usize> {
        self.require_writable()?;
        if ids.len() > 200 {
            return Err(invalid("at most 200 memory ids per usage event"));
        }
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        let mut unique: Vec<String> = Vec::new();
        for id in ids {
            if !unique.contains(id) && self.get(id, &viewer)?.is_some() {
                unique.push(id.clone());
            }
        }
        let now = ctx.now_iso.clone();
        let conn = &self.conn;
        conn.execute_batch("BEGIN IMMEDIATE").map_err(sql_err)?;
        let result = (|| -> rusqlite::Result<usize> {
            let sql = match kind {
                UsageKind::Injection => {
                    "UPDATE memories SET data = json_set(data,
                       '$.injectionCount', COALESCE(json_extract(data, '$.injectionCount'), 0) + 1,
                       '$.lastAccessedAt', ?1)
                     WHERE id = ?2 AND status != 'deleted' AND json_valid(data)"
                }
                UsageKind::Use => {
                    "UPDATE memories SET data = json_set(data,
                       '$.useCount', COALESCE(json_extract(data, '$.useCount'), 0) + 1,
                       '$.lastUsedAt', ?1,
                       '$.lastAccessedAt', ?1)
                     WHERE id = ?2 AND status != 'deleted' AND json_valid(data)"
                }
            };
            let mut n = 0;
            for id in &unique {
                n += conn.execute(sql, params![now, id])?;
            }
            let (event, key) = match kind {
                UsageKind::Injection => ("memory.injected", "trigger"),
                UsageKind::Use => ("memory.used", "source"),
            };
            audit(
                conn,
                event,
                &now,
                json!({"details": {"memoryIds": unique, key: label, "sessionId": ctx.session_id, "principal": ctx.principal_id}}),
            )?;
            Ok(n)
        })();
        crate::schema::finish(conn, result).map_err(sql_err)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageKind {
    Injection,
    Use,
}

fn sorted_key(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys.iter().map(|k| format!("{k:?}:{}", m[*k])).collect();
            format!("{{{}}}", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// `anchorsChanged`: structural comparison of normalized anchors.
fn anchors_changed(a: &[MemoryAnchor], b: &[MemoryAnchor]) -> bool {
    let ka: HashSet<String> = a.iter().map(|x| sorted_key(&to_value(x))).collect();
    let kb: HashSet<String> = b.iter().map(|x| sorted_key(&to_value(x))).collect();
    ka != kb
}

#[allow(clippy::too_many_arguments)]
fn find_near_duplicate(
    conn: &Connection,
    scope: &str,
    audience_key: Option<&str>,
    kind: &str,
    text: &str,
    anchors: &[MemoryAnchor],
    validity_key: &str,
    owner: Option<&str>,
) -> StoreResult<Option<SageRecord>> {
    let session = if owner.is_some() {
        " AND owner_session_id = ?3"
    } else {
        ""
    };
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut push = |sql: &str, extra: Option<&str>| -> rusqlite::Result<()> {
        let mut s = conn.prepare(sql)?;
        let mut args: Vec<rusqlite::types::Value> = vec![
            scope.to_string().into(),
            audience_key.map(str::to_string).into(),
        ];
        if let Some(o) = owner {
            args.push(o.to_string().into());
        }
        if let Some(e) = extra {
            args.push(e.to_string().into());
        }
        let it = s.query_map(rusqlite::params_from_iter(args), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        for r in it {
            let r = r?;
            if !rows.iter().any(|(id, _)| *id == r.0) {
                rows.push(r);
            }
        }
        Ok(())
    };
    push(
        &format!(
            "SELECT id, data FROM memories WHERE status IN ('active','stale') AND scope = ?1 AND audience IS ?2{session}
             ORDER BY importance DESC, updated_at DESC LIMIT 64"
        ),
        None,
    )
    .map_err(sql_err)?;
    let terms = crate::store::fts_prefix_terms(text);
    if !terms.is_empty() {
        let n = if owner.is_some() { 4 } else { 3 };
        let sql = format!(
            "SELECT m.id, m.data FROM memories_fts f CROSS JOIN memories m ON m.rowid = f.rowid
              WHERE m.status IN ('active','stale') AND m.scope = ?1 AND m.audience IS ?2{}
                AND memories_fts MATCH ?{n} ORDER BY bm25(memories_fts) ASC LIMIT 32",
            session.replace("owner_session_id", "m.owner_session_id")
        );
        // FTS failures are ignored like the baseline's `catch {}`.
        let _ = push(&sql, Some(&terms.join(" OR ")));
    }
    let mut best: Option<(SageRecord, f64)> = None;
    for (_, data) in rows {
        let Some(c) = decode_memory(&data) else {
            continue;
        };
        let ck = c
            .validity
            .as_ref()
            .map(|v| to_value(v).to_string())
            .unwrap_or_else(|| "null".into());
        if ck != validity_key
            || !similarity::is_near_duplicate(text, kind, anchors, &c)
            || similarity::is_possibly_contradictory(text, &c.text)
        {
            continue;
        }
        let score = c.importance * 2.0 + c.confidence + c.freshness;
        if best.as_ref().is_none_or(|(_, s)| score > *s) {
            best = Some((c, score));
        }
    }
    Ok(best.map(|(c, _)| c))
}

fn append_feedback(
    conn: &Connection,
    mut m: Map<String, Value>,
    existing: &SageRecord,
    fb: &Value,
    ctx: &MutationCtx,
    now: &str,
) -> StoreResult<MutationOutcome> {
    const VERDICTS: &[&str] = &["useful", "outdated", "incorrect", "irrelevant", "uncertain"];
    let verdict = fb.get("verdict").and_then(Value::as_str).unwrap_or("");
    if !VERDICTS.contains(&verdict) {
        return Err(invalid("Invalid SAGE feedback verdict."));
    }
    let observed = fb
        .get("observedRevision")
        .and_then(Value::as_i64)
        .filter(|r| *r >= 1)
        .ok_or_else(|| invalid("SAGE feedback requires a positive observedRevision."))?;
    if observed != existing.revision {
        return Err(ErrorBody::new(
            ErrorCode::RevisionConflict,
            "SAGE revision changed; read the current memory and reassess before submitting feedback.",
        ));
    }
    let evidence = fb.get("evidence").and_then(Value::as_str).unwrap_or("");
    if evidence.trim().is_empty() || js_length(evidence) > 2000 {
        return Err(invalid(
            "SAGE feedback evidence must contain 1–2000 characters.",
        ));
    }
    // Server-attributed session; a client cannot write another session's review.
    let mut item = Map::new();
    item.insert("verdict".into(), json!(verdict));
    item.insert("observedRevision".into(), json!(observed));
    item.insert("evidence".into(), json!(evidence.trim()));
    if let Some(s) = &ctx.session_id {
        item.insert("sessionId".into(), json!(s));
    }
    item.insert("at".into(), json!(now));
    item.insert("principalId".into(), json!(ctx.principal_id));
    let history: Vec<Value> = existing.feedback.clone().unwrap_or_default();
    let dup = history.iter().any(|h| {
        h.get("observedRevision") == item.get("observedRevision")
            && h.get("verdict") == item.get("verdict")
            && h.get("evidence") == item.get("evidence")
            && h.get("sessionId") == item.get("sessionId")
    });
    if dup {
        return Ok(MutationOutcome {
            outcome: "memory-committed".into(),
            id: existing.id.clone(),
            revision: Some(existing.revision),
            replayed: false,
            record: to_value(existing),
        });
    }
    let mut next = history;
    next.push(Value::Object(item.clone()));
    let skip = next.len().saturating_sub(MAX_FEEDBACK);
    m.insert(
        "feedback".into(),
        Value::Array(next.into_iter().skip(skip).collect()),
    );
    // Feedback is not a content revision (baseline keeps revision/updatedAt).
    let rec = upsert_raw(conn, &mut m)?;
    audit(
        conn,
        "memory.feedback",
        now,
        json!({"memoryId": existing.id, "details": item}),
    )
    .map_err(sql_err)?;
    Ok(MutationOutcome {
        outcome: "memory-committed".into(),
        id: rec.id.clone(),
        revision: Some(rec.revision),
        replayed: false,
        record: to_value(&rec),
    })
}

fn tombstone_cleanup(
    conn: &Connection,
    id: &str,
    now: &str,
    force: bool,
    updated: &SageRecord,
    ctx: &MutationCtx,
) -> StoreResult<()> {
    let node = format!("mem:{id}");
    let mut targets = HashSet::new();
    targets.insert(id.to_string());
    clean_referencing(conn, &targets, now)?;
    let removed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE (from_node = ?1 OR to_node = ?1) AND relation != 'related_to'",
            [&node],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    cascade_delete_edges(conn, &node)?;
    audit(
        conn,
        "memory.deleted",
        now,
        json!({"memoryId": id, "reason": "Memory status changed to deleted.", "details": {
            "force": force, "persistence": updated.persistence.as_deref().unwrap_or("long_lived"),
            "removedEdges": removed, "principal": ctx.principal_id,
        }}),
    )
    .map_err(sql_err)
}

#[allow(dead_code)]
fn _unused(_: &str) -> Option<String> {
    anchor_node(&MemoryAnchor::default())
}
