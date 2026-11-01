//! Legacy `MemoryStore` bulk mutations: `forget`, `consolidate`, `clear`
//! (baseline `sqlite-store-legacy-{forget,consolidate,clear}.ts`).
//!
//! Semantics follow the baseline row for row, with one deliberate
//! deviation: rows are selected under the caller's session rule. The
//! baseline operates on every row of the legacy scope, so a `clear` from one
//! session would tombstone another session's private rows; here a caller
//! can only affect rows it can see (WM-INV hidden/absent rule).
//!
//! Records are rewritten by patching the stored JSON object (unknown fields
//! and `null` survive); every operation runs in one receipt transaction.

use std::collections::HashSet;

use rusqlite::{Connection, params_from_iter};
use serde_json::{Map, Value, json};
use wrongmemory_contracts::{ErrorBody, ErrorCode, MutationOutcome, SageRecord};

use crate::mutate::{cascade_delete_edges, clean_referencing, sync_edges, upsert_raw};
use crate::store::{
    MutationCtx, SageStore, StoreResult, Viewer, audit, decode_memory, fingerprint,
    legacy_scope_for, session_clause, sql_err,
};
use crate::text::normalize_text_key;

/// A scanned record with its stored JSON object.
type Member = (SageRecord, Map<String, Value>);

const LEGACY_SCOPES: &[&str] = &["project-agents", "project-memory", "user-memory"];

fn invalid(msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::InvalidRequest, msg)
}

fn check_scope(scope: &str) -> StoreResult<()> {
    if LEGACY_SCOPES.contains(&scope) {
        Ok(())
    } else {
        Err(invalid(format!(
            "unknown legacy memory scope; expected one of {}",
            LEGACY_SCOPES.join(", ")
        )))
    }
}

/// `legacyToSageScope`.
fn sage_scope(scope: &str) -> &'static str {
    if scope == "user-memory" {
        "user"
    } else {
        "project"
    }
}

/// `memory.legacyScope ?? sageToLegacyScope(memory.scope)`.
fn effective_legacy_scope(m: &SageRecord) -> String {
    m.legacy_scope
        .clone()
        .unwrap_or_else(|| legacy_scope_for(&m.scope).to_string())
}

fn is_permanent(m: &SageRecord) -> bool {
    m.persistence.as_deref() == Some("permanent")
}

/// `matchesLegacyForget`: substring match over id, text, tags and anchor
/// path/symbol/command/role, lower-cased and space-joined.
fn matches_forget(m: &SageRecord, normalized_query: &str) -> bool {
    let mut parts: Vec<&str> = vec![&m.id, &m.text];
    parts.extend(m.tags.iter().map(String::as_str));
    for a in &m.anchors {
        for v in [&a.path, &a.symbol, &a.command, &a.role] {
            if let Some(s) = v.as_deref() {
                parts.push(s);
            }
        }
    }
    parts.join(" ").to_lowercase().contains(normalized_query)
}

/// Rows matching `status_sql` and the optional legacy scope filter, under
/// the session rule, in the baseline's (unordered) scan order.
fn scoped_rows(
    conn: &Connection,
    status_sql: &str,
    scope: Option<&str>,
    viewer: &Viewer,
) -> StoreResult<Vec<String>> {
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    let mut sql = format!("SELECT data FROM memories WHERE {status_sql}");
    if let Some(s) = scope {
        args.push(sage_scope(s).to_string().into());
        args.push(s.to_string().into());
        sql.push_str(" AND (scope = ?1 OR legacy_scope = ?2)");
    }
    args.push(viewer.session_id.clone().into());
    sql.push_str(" AND ");
    sql.push_str(&session_clause("", viewer, args.len()));
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(params_from_iter(args), |r| r.get::<_, String>(0))
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<String>>>()
        .map_err(sql_err)?;
    Ok(rows)
}

/// Tombstone one memory the way legacy forget/clear do: status `deleted`,
/// relationship fields dropped, references cleaned, edges cascaded.
///
/// Like the baseline, the record is written from the snapshot taken when the
/// scope was scanned (`data`), so a reference cleanup applied to it earlier
/// in the same pass does not add a second revision bump. Live counters are
/// still merged from the current row by `upsert_raw`.
fn tombstone(conn: &Connection, data: &str, rec: &SageRecord, now: &str) -> StoreResult<()> {
    let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(data) else {
        return Ok(());
    };
    let id = rec.id.as_str();
    m.insert("status".into(), json!("deleted"));
    m.remove("supersedes");
    m.remove("contradicts");
    m.remove("supersededBy");
    m.insert("revision".into(), json!(rec.revision + 1));
    m.insert("updatedAt".into(), json!(now));
    upsert_raw(conn, &mut m)?;
    let mut targets = HashSet::new();
    targets.insert(id.to_string());
    clean_referencing(conn, &targets, now)?;
    cascade_delete_edges(conn, &format!("mem:{id}"))?;
    Ok(())
}

fn outcome(record: Value) -> MutationOutcome {
    MutationOutcome {
        outcome: "memory-committed".into(),
        id: String::new(),
        revision: None,
        replayed: false,
        record,
    }
}

impl SageStore {
    /// `forget(query, scope)`: tombstone every visible non-deleted memory of
    /// the legacy scope whose searchable text contains `query`
    /// (case-insensitive). Permanent memories are skipped and audited.
    /// Returns `{removed}`.
    pub fn legacy_forget(
        &mut self,
        query: &str,
        scope: &str,
        ctx: &MutationCtx,
    ) -> StoreResult<MutationOutcome> {
        self.require_writable()?;
        check_scope(scope)?;
        let normalized = query.trim().to_lowercase();
        if normalized.is_empty() {
            return Ok(outcome(json!({"removed": 0})));
        }
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        let fp = fingerprint(&json!({"query": query, "scope": scope}));
        let now = ctx.now_iso.clone();
        let scope = scope.to_string();
        self.with_receipt(ctx, "memory.legacyForget", &fp, |conn| {
            let mut removed = 0usize;
            let mut skipped: Vec<String> = Vec::new();
            for data in scoped_rows(conn, "status != 'deleted'", Some(&scope), &viewer)? {
                let Some(m) = decode_memory(&data) else {
                    continue;
                };
                if effective_legacy_scope(&m) != scope || !matches_forget(&m, &normalized) {
                    continue;
                }
                if is_permanent(&m) {
                    skipped.push(m.id);
                    continue;
                }
                tombstone(conn, &data, &m, &now)?;
                removed += 1;
            }
            if removed > 0 {
                audit(
                    conn,
                    "memory.deleted",
                    &now,
                    json!({"reason": query, "details": {"removed": removed, "scope": scope}}),
                )
                .map_err(sql_err)?;
            }
            if !skipped.is_empty() {
                audit(
                    conn,
                    "memory.forget_skipped_permanent",
                    &now,
                    json!({"reason": query, "details": {"skipped": skipped, "scope": scope}}),
                )
                .map_err(sql_err)?;
            }
            Ok(outcome(json!({"removed": removed})))
        })
    }

    /// `clear(scope?)`: tombstone every visible non-deleted, non-permanent
    /// memory (of the legacy scope, or all). Returns
    /// `{deleted, skippedPermanent}`.
    pub fn legacy_clear(
        &mut self,
        scope: Option<&str>,
        ctx: &MutationCtx,
    ) -> StoreResult<MutationOutcome> {
        self.require_writable()?;
        if let Some(s) = scope {
            check_scope(s)?;
        }
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        let fp = fingerprint(&json!({"scope": scope}));
        let now = ctx.now_iso.clone();
        let scope = scope.map(str::to_string);
        self.with_receipt(ctx, "memory.legacyClear", &fp, |conn| {
            let mut cleared: Vec<String> = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
            for data in scoped_rows(conn, "status != 'deleted'", scope.as_deref(), &viewer)? {
                let Some(m) = decode_memory(&data) else {
                    continue;
                };
                if m.id.is_empty() || m.created_at.is_empty() || m.updated_at.is_empty() {
                    continue;
                }
                if scope
                    .as_deref()
                    .is_some_and(|s| effective_legacy_scope(&m) != s)
                {
                    continue;
                }
                if is_permanent(&m) {
                    skipped.push(m.id);
                    continue;
                }
                tombstone(conn, &data, &m, &now)?;
                cleared.push(m.id);
            }
            let scope_label = scope.clone().unwrap_or_else(|| "all".into());
            if !skipped.is_empty() {
                audit(
                    conn,
                    "memory.clear_skipped_permanent",
                    &now,
                    json!({"details": {"skipped": skipped, "scope": scope_label}}),
                )
                .map_err(sql_err)?;
            }
            audit(
                conn,
                "memory.bulk_clear_completed",
                &now,
                json!({"details": {"scope": scope_label, "deleted": cleared.len(),
                                   "skippedPermanent": skipped.len()}}),
            )
            .map_err(sql_err)?;
            Ok(outcome(
                json!({"deleted": cleared.len(), "skippedPermanent": skipped.len()}),
            ))
        })
    }

    /// `consolidate(scope)`: group visible active/stale memories of the
    /// legacy scope without validity by normalized text; in every group with
    /// two or more non-permanent members, the highest importance (then
    /// confidence) member keeps the union of tags, anchors and sources and
    /// supersedes the rest. Returns `{removed}`.
    pub fn legacy_consolidate(
        &mut self,
        scope: &str,
        ctx: &MutationCtx,
    ) -> StoreResult<MutationOutcome> {
        self.require_writable()?;
        check_scope(scope)?;
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        let fp = fingerprint(&json!({"scope": scope}));
        let now = ctx.now_iso.clone();
        let scope = scope.to_string();
        self.with_receipt(ctx, "memory.legacyConsolidate", &fp, |conn| {
            // Insertion-ordered groups (the baseline's Map).
            let mut keys: Vec<String> = Vec::new();
            let mut groups: std::collections::HashMap<String, Vec<Member>> = Default::default();
            for data in scoped_rows(conn, "status IN ('active','stale')", Some(&scope), &viewer)? {
                let Some(m) = decode_memory(&data) else {
                    continue;
                };
                if m.validity.is_some() || effective_legacy_scope(&m) != scope {
                    continue;
                }
                let Ok(Value::Object(raw)) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                // Baseline groups by normalized text alone. Session rows are
                // grouped only with rows of the same owning session, so a
                // merge never moves private content into a shared row or
                // makes a shared row point at a private one.
                let key = if m.scope == "session" {
                    format!(
                        "session {} {}",
                        m.owner_session_id.as_deref().unwrap_or(""),
                        normalize_text_key(&m.text)
                    )
                } else {
                    normalize_text_key(&m.text)
                };
                if !groups.contains_key(&key) {
                    keys.push(key.clone());
                }
                groups.entry(key).or_default().push((m, raw));
            }
            let mut removed = 0usize;
            for key in keys {
                let group = &groups[&key];
                if group.len() < 2 {
                    continue;
                }
                let mutable: Vec<&Member> =
                    group.iter().filter(|(m, _)| !is_permanent(m)).collect();
                if mutable.len() < 2 {
                    continue;
                }
                let mut sorted = mutable.clone();
                // Stable, like Array.prototype.sort.
                sorted.sort_by(|(a, _), (b, _)| {
                    b.importance
                        .total_cmp(&a.importance)
                        .then(b.confidence.total_cmp(&a.confidence))
                });
                let (keeper, keeper_raw) = sorted[0];
                let duplicates = &sorted[1..];

                // Unions over `mutable` (group order), as the baseline.
                let mut tags: Vec<Value> = Vec::new();
                let mut seen_tags = HashSet::new();
                let mut anchors: Vec<(String, Value)> = Vec::new();
                let mut sources: Vec<(String, Value)> = Vec::new();
                for (_, raw) in &mutable {
                    for t in raw
                        .get("tags")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if seen_tags.insert(t.to_string()) {
                            tags.push(t.clone());
                        }
                    }
                    for a in raw
                        .get("anchors")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let f = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("");
                        let k = format!(
                            "{}\0{}\0{}\0{}\0{}",
                            f("type"),
                            f("path"),
                            f("symbol"),
                            f("command"),
                            f("role")
                        );
                        // Map.set: first position, last value.
                        match anchors.iter_mut().find(|(ek, _)| *ek == k) {
                            Some(slot) => slot.1 = a.clone(),
                            None => anchors.push((k, a.clone())),
                        }
                    }
                    for s in raw
                        .get("sources")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let k = s.to_string();
                        match sources.iter_mut().find(|(ek, _)| *ek == k) {
                            Some(slot) => slot.1 = s.clone(),
                            None => sources.push((k, s.clone())),
                        }
                    }
                }
                let mut supersedes: Vec<String> = Vec::new();
                for id in keeper
                    .supersedes
                    .iter()
                    .flatten()
                    .cloned()
                    .chain(duplicates.iter().map(|(d, _)| d.id.clone()))
                {
                    if !supersedes.contains(&id) {
                        supersedes.push(id);
                    }
                }
                let mut k = keeper_raw.clone();
                k.insert("tags".into(), Value::Array(tags));
                k.insert(
                    "anchors".into(),
                    Value::Array(anchors.into_iter().map(|(_, v)| v).collect()),
                );
                k.insert(
                    "sources".into(),
                    Value::Array(sources.into_iter().map(|(_, v)| v).collect()),
                );
                k.insert("supersedes".into(), json!(supersedes));
                k.insert("revision".into(), json!(keeper.revision + 1));
                k.insert("updatedAt".into(), json!(now));
                let rec = upsert_raw(conn, &mut k)?;
                sync_edges(conn, &rec, &now)?;
                for (dup, dup_raw) in duplicates {
                    let mut d = dup_raw.clone();
                    d.insert("status".into(), json!("superseded"));
                    d.insert("supersededBy".into(), json!(keeper.id));
                    d.insert("revision".into(), json!(dup.revision + 1));
                    d.insert("updatedAt".into(), json!(now));
                    let rec = upsert_raw(conn, &mut d)?;
                    sync_edges(conn, &rec, &now)?;
                    removed += 1;
                }
            }
            audit(
                conn,
                "memory.consolidation_completed",
                &now,
                json!({"details": {"scope": scope, "removed": removed}}),
            )
            .map_err(sql_err)?;
            Ok(outcome(json!({"removed": removed})))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrongmemory_contracts::{MemorySource, RememberParams};

    fn ctx(session: Option<&str>) -> MutationCtx {
        MutationCtx {
            principal_id: "host".into(),
            client_label: "t".into(),
            session_id: session.map(str::to_string),
            idempotency_key: None,
            now_iso: "2026-11-01T00:00:01.000Z".into(),
        }
    }

    fn remember(store: &mut SageStore, text: &str, session: Option<&str>) -> String {
        let p = RememberParams {
            text: text.into(),
            scope: session.map(|_| "session".to_string()),
            sources: vec![MemorySource {
                kind: "user".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        store.remember(&p, &ctx(session)).unwrap().id
    }

    fn status(store: &SageStore, id: &str) -> String {
        store
            .conn
            .query_row("SELECT status FROM memories WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// Deviation from the baseline: bulk legacy mutations only touch rows
    /// the caller can see, so one session cannot erase another's private
    /// memories.
    #[test]
    fn bulk_mutations_respect_the_session_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("sage.db");
        SageStore::create_managed(&db, "2026-11-01T00:00:00.000Z").unwrap();
        let (mut store, _) = SageStore::open(&db, true, None, "2026-11-01T00:00:00.000Z").unwrap();
        let public = remember(&mut store, "Public note about the deploy pipeline.", None);
        let private = remember(
            &mut store,
            "Private scratch about the deploy pipeline.",
            Some("s1"),
        );

        let out = store
            .legacy_forget("deploy", "project-memory", &ctx(Some("s2")))
            .unwrap();
        assert_eq!(out.record["removed"], 1);
        assert_eq!(status(&store, &public), "deleted");
        assert_eq!(status(&store, &private), "active");

        let out = store.legacy_clear(None, &ctx(None)).unwrap();
        assert_eq!(out.record["deleted"], 0);
        assert_eq!(status(&store, &private), "active");

        let out = store.legacy_clear(None, &ctx(Some("s1"))).unwrap();
        assert_eq!(out.record["deleted"], 1);
        assert_eq!(status(&store, &private), "deleted");

        let err = store.legacy_clear(Some("bogus"), &ctx(None)).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }

    /// Consolidation never merges a session row with a shared row: neither
    /// direction may move private content or ids into shared records.
    #[test]
    fn consolidate_keeps_session_rows_apart_from_shared_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("sage.db");
        SageStore::create_managed(&db, "2026-11-01T00:00:00.000Z").unwrap();
        let (mut store, _) = SageStore::open(&db, true, None, "2026-11-01T00:00:00.000Z").unwrap();
        let text = "Cache keys include the tenant id.";
        let shared = remember(&mut store, text, None);
        let p = RememberParams {
            text: text.into(),
            scope: Some("session".into()),
            importance: Some(0.95),
            tags: vec!["privatetag".into()],
            sources: vec![MemorySource {
                kind: "user".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let private = store.remember(&p, &ctx(Some("s1"))).unwrap().id;
        assert_ne!(shared, private);
        let out = store
            .legacy_consolidate("project-memory", &ctx(Some("s1")))
            .unwrap();
        assert_eq!(out.record["removed"], 0);
        let data: String = store
            .conn
            .query_row("SELECT data FROM memories WHERE id = ?1", [&shared], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            !data.contains(&private) && !data.contains("privatetag"),
            "{data}"
        );
        assert_eq!(status(&store, &shared), "active");
        assert_eq!(status(&store, &private), "active");
    }

    #[test]
    fn invalid_scope_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("sage.db");
        SageStore::create_managed(&db, "2026-11-01T00:00:00.000Z").unwrap();
        let (mut store, _) = SageStore::open(&db, true, None, "2026-11-01T00:00:00.000Z").unwrap();
        let err = store.legacy_clear(Some("bogus"), &ctx(None)).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }
}
