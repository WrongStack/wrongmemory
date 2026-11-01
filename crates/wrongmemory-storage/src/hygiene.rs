//! Lifecycle hygiene (WM-506 partial, WM-507), ported from
//! `sqlite-store-hygiene.ts`, `sqlite-hygiene-verification.ts`,
//! `sqlite-hygiene-anchors.ts`, `shared/stale-reason.ts`,
//! `shared/candidate-dedupe.ts` and `shared/semantic-rewrite.ts` at the
//! baseline commit.
//!
//! Phases, in baseline order: structural verification (existence only) and
//! verification-stale reactivation; exact dedup; near-duplicate union-find
//! (bucket cap 80); contradiction marking with `memory_review` candidates;
//! review candidates and session GC; optional tombstone purge.
//!
//! Differences from the baseline, all deliberate:
//! * Verification never executes anything: it canonicalizes anchor paths,
//!   requires them to stay under the real project root, and stats them.
//!   `verifyDepth` `content`/`git` are not ported and behave as `existence`.
//! * When the project root is unknown or unreadable, verification is
//!   skipped (evidence unknown) instead of demoting every anchored memory.
//! * Literal validity checks read at most [`MAX_VALIDITY_BYTES`] per file; a
//!   missing or unreadable file is "unknown" and never demotes a memory.
//! * All write phases run in ONE IMMEDIATE transaction. `dry_run` (a
//!   WrongMemory extension; the baseline has none) runs the same pass and
//!   rolls it back, returning the report it would have produced.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use wrongmemory_contracts::{ErrorBody, ErrorCode, MemoryAnchor, SageRecord};

use crate::MutationCtx;
use crate::mutate::{cascade_delete_edges, clean_referencing, read_raw, sync_edges, upsert_raw};
use crate::recovery::all_memories;
use crate::rules;
use crate::similarity::{is_near_duplicate, is_possibly_contradictory};
use crate::store::{SageStore, StoreResult, audit, sql_err};
use crate::text::{js_length, normalize_text, normalize_text_key, parse_iso_ms};

const NEAR_DUP_BUCKET_CAP: usize = 80;
const CRITICAL_IMPORTANCE_FLOOR: f64 = 0.9;
const DEFAULT_SESSION_RETENTION_DAYS: f64 = 7.0;
const DAY_MS: f64 = 86_400_000.0;
const REVIEW_SUPPRESSION_MS: i64 = 90 * 86_400_000;
const LEGACY_VERIFICATION_STALE_WINDOW_MS: i64 = 15 * 60_000;
/// Upper bound on distinct anchor paths stat'ed in one pass.
const MAX_VERIFY_PATHS: usize = 50_000;
/// Upper bound on bytes read per literal validity check.
pub const MAX_VALIDITY_BYTES: u64 = 4 * 1024 * 1024;

/// `SageHygieneOptions` plus the `dryRun` extension.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HygieneOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_retention_days: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_low_confidence_after_days: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_unused_after_days: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unused_min_injections: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_depth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near_dedup: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge_deleted_after_days: Option<f64>,
    /// WrongMemory extension: compute the report, write nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// `SageHygieneReport` (`dryRun` is a WrongMemory extension).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HygieneReport {
    pub started_at: String,
    pub completed_at: String,
    pub examined: usize,
    pub deduplicated: usize,
    pub superseded: usize,
    pub contradicted: usize,
    pub staled: usize,
    pub review_candidates_created: usize,
    pub archived: usize,
    pub archived_unused: usize,
    pub deleted: usize,
    pub purged_deleted: usize,
    pub verified: usize,
    pub reactivated: usize,
    pub transitive_merges: usize,
    #[serde(default)]
    pub dry_run: bool,
}

fn ms(s: Option<&str>) -> Option<i64> {
    s.and_then(parse_iso_ms)
}

/// Optional stamps that do not parse are treated as absent (codec rule).
fn opt_stamp(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|v| parse_iso_ms(v).is_some())
}

fn is_permanent(m: &SageRecord) -> bool {
    m.persistence.as_deref() == Some("permanent")
}

/// `hygieneScopeKey`.
fn scope_key(m: &SageRecord) -> String {
    if m.scope == "session" {
        format!("session:{}", m.owner_session_id.as_deref().unwrap_or(""))
    } else {
        m.scope.clone()
    }
}

/// `JSON.stringify(normalizeAudience(m.audience) ?? null)`.
fn audience_key(m: &SageRecord) -> String {
    m.audience
        .as_ref()
        .and_then(|a| {
            rules::normalize_audience(&serde_json::to_value(a).unwrap_or(Value::Null))
                .ok()
                .flatten()
        })
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".into())
}

/// `isVerificationStale`.
pub fn is_verification_stale(m: &SageRecord) -> bool {
    if m.status != "stale" {
        return false;
    }
    if let Some(r) = &m.stale_reason {
        return r == "verification";
    }
    let (Some(u), Some(v)) = (ms(Some(&m.updated_at)), ms(m.last_verified_at.as_deref())) else {
        return false;
    };
    let gap = u - v;
    (0..=LEGACY_VERIFICATION_STALE_WINDOW_MS).contains(&gap)
}

fn is_path_anchor(kind: &str) -> bool {
    matches!(
        kind,
        "file" | "symbol" | "test" | "git" | "directory" | "package"
    )
}

/// `existenceProvesAnchors`.
fn existence_proves(anchors: &[MemoryAnchor]) -> bool {
    !anchors.is_empty()
        && anchors.iter().all(|a| {
            a.path.as_deref().is_some_and(|p| !p.is_empty())
                && matches!(a.kind.as_str(), "file" | "test" | "directory" | "package")
                && !a.extra.contains_key("contentHash")
                && !a.extra.contains_key("gitBlobHash")
                && a.symbol.is_none()
        })
}

/// Canonical path under the real root, or `None` when missing / escaping.
///
/// Stored anchor paths are not trusted (adopted stores may hold absolute or
/// UNC paths): anything that is not lexically inside the root is rejected
/// before the filesystem is touched, so no network share is ever contacted.
fn resolve_under(root: &Path, real_root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.starts_with("\\\\") || rel.starts_with("//") {
        return None;
    }
    let joined = crate::wrongstack_paths::node_resolve(&root.join(rel));
    if !crate::paths::lexically_inside(&crate::paths::resolved_root_of(root), &joined)
        && !crate::paths::lexically_inside(real_root, &joined)
    {
        return None;
    }
    let real = std::fs::canonicalize(&joined).ok()?;
    real.starts_with(real_root).then_some(real)
}

fn anchors_present(root: &Path, real_root: &Path, anchors: &[MemoryAnchor]) -> bool {
    anchors.iter().all(|a| {
        let Some(real) = a
            .path
            .as_deref()
            .and_then(|p| resolve_under(root, real_root, p))
        else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(&real) else {
            return false;
        };
        if matches!(a.kind.as_str(), "directory" | "package") {
            meta.is_dir()
        } else {
            meta.is_file()
        }
    })
}

/// Literal `source_contains` checks: `true` when some check is definitely
/// not satisfied (file read, literal absent). Unreadable/missing = unknown.
fn validity_fails(root: &Path, real_root: &Path, m: &SageRecord) -> bool {
    let Some(checks) = m.validity.as_ref().and_then(|v| v.checks.as_ref()) else {
        return false;
    };
    checks.iter().any(|c| {
        if c.kind != "source_contains" {
            return false;
        }
        let Some(real) = resolve_under(root, real_root, &c.path) else {
            return false;
        };
        let Ok(file) = std::fs::File::open(&real) else {
            return false;
        };
        let mut buf = Vec::new();
        if file.take(MAX_VALIDITY_BYTES).read_to_end(&mut buf).is_err() {
            return false;
        }
        !String::from_utf8_lossy(&buf).contains(c.text.as_str())
    })
}

/// `applySemanticChange` over a raw record: patch, revision+1, updatedAt.
/// A `None` value removes the key (JS `undefined`).
pub(crate) fn semantic(
    conn: &Connection,
    id: &str,
    patch: Vec<(&str, Option<Value>)>,
    now: &str,
) -> StoreResult<Option<SageRecord>> {
    let Some((mut m, cur)) = read_raw(conn, id).map_err(sql_err)? else {
        return Ok(None);
    };
    for (k, v) in patch {
        match v {
            Some(v) => {
                m.insert(k.into(), v);
            }
            None => {
                m.remove(k);
            }
        }
    }
    m.insert("revision".into(), json!(cur.revision + 1));
    m.insert("updatedAt".into(), json!(now));
    Ok(Some(upsert_raw(conn, &mut m)?))
}

fn raw_array(conn: &Connection, id: &str, key: &str) -> StoreResult<Vec<Value>> {
    Ok(read_raw(conn, id)
        .map_err(sql_err)?
        .and_then(|(m, _)| m.get(key).and_then(Value::as_array).cloned())
        .unwrap_or_default())
}

/// `JSON.stringify(a, Object.keys(a).sort())`: sorted top-level keys.
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

fn keeper_order(a: &SageRecord, b: &SageRecord) -> std::cmp::Ordering {
    (is_permanent(b) as u8)
        .cmp(&(is_permanent(a) as u8))
        .then(
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
        .then(
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
        .then(a.created_at.cmp(&b.created_at))
}

fn supersede_edge(conn: &Connection, keeper: &str, dup: &str, now: &str) {
    let _ = conn.execute(
        "INSERT INTO edges (from_node, to_node, relation, weight, created_at)
         VALUES (?1, ?2, 'supersedes', 1, ?3)
         ON CONFLICT(from_node, to_node, relation) DO UPDATE SET weight = MAX(weight, excluded.weight)",
        params![format!("mem:{keeper}"), format!("mem:{dup}"), now],
    );
}

/// Merge a keeper with duplicates: union tags/anchors/sources, supersede dups.
fn merge_group(
    conn: &Connection,
    keeper: &SageRecord,
    set: &[&SageRecord],
    duplicates: &[&SageRecord],
    text: Option<&str>,
    now: &str,
) -> StoreResult<()> {
    let mut tags: Vec<String> = Vec::new();
    let mut anchors: Vec<Value> = Vec::new();
    let mut anchor_keys = HashSet::new();
    let mut sources: Vec<Value> = Vec::new();
    let mut source_keys = HashSet::new();
    for m in set {
        for t in &m.tags {
            if !tags.contains(t) {
                tags.push(t.clone());
            }
        }
        for a in raw_array(conn, &m.id, "anchors")? {
            if anchor_keys.insert(sorted_key(&a)) {
                anchors.push(a);
            }
        }
        for s in raw_array(conn, &m.id, "sources")? {
            if source_keys.insert(s.to_string()) {
                sources.push(s);
            }
        }
    }
    let mut supersedes = keeper.supersedes.clone().unwrap_or_default();
    for d in duplicates {
        if !supersedes.contains(&d.id) {
            supersedes.push(d.id.clone());
        }
    }
    let mut patch = vec![
        ("tags", Some(json!(tags))),
        ("anchors", Some(Value::Array(anchors))),
        ("sources", Some(Value::Array(sources))),
        ("supersedes", Some(json!(supersedes))),
    ];
    if let Some(t) = text {
        patch.insert(0, ("text", Some(json!(t))));
    }
    if let Some(updated) = semantic(conn, &keeper.id, patch, now)? {
        sync_edges(conn, &updated, now)?;
    }
    for d in duplicates {
        if let Some(sup) = semantic(
            conn,
            &d.id,
            vec![
                ("status", Some(json!("superseded"))),
                ("supersededBy", Some(json!(keeper.id))),
            ],
            now,
        )? {
            sync_edges(conn, &sup, now)?;
        }
        supersede_edge(conn, &keeper.id, &d.id, now);
    }
    Ok(())
}

fn list_status(conn: &Connection, status: &str) -> StoreResult<Vec<SageRecord>> {
    Ok(all_memories(conn)?
        .into_iter()
        .filter(|m| m.status == status)
        .collect())
}

/// Insert a review candidate the way `addCandidate` does (secret guard, then
/// `INSERT OR REPLACE` with canonical text). Unsafe content is skipped.
fn add_candidate(conn: &Connection, c: Map<String, Value>) -> StoreResult<bool> {
    let v = Value::Object(c);
    if rules::reject_if_unsafe(&v).is_err() {
        return Ok(false);
    }
    let text = v.get("text").and_then(Value::as_str).unwrap_or("");
    conn.execute(
        "INSERT OR REPLACE INTO candidates (id, data, status, created_at, updated_at, canonical_text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            v["id"].as_str(),
            v.to_string(),
            v["status"].as_str(),
            v["createdAt"].as_str(),
            v["updatedAt"].as_str(),
            normalize_text_key(text),
        ],
    )
    .map_err(sql_err)?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn review_candidate(
    id: String,
    target: &SageRecord,
    scope: &str,
    confidence: f64,
    importance: f64,
    reason: &str,
    action: &str,
    tags: Vec<String>,
    anchors: Value,
    now: &str,
) -> Map<String, Value> {
    let mut c = Map::new();
    c.insert("id".into(), json!(id));
    c.insert("schemaVersion".into(), json!(1));
    c.insert("targetMemoryId".into(), json!(target.id));
    c.insert("text".into(), json!(target.text));
    c.insert("kind".into(), json!("memory_review"));
    c.insert("status".into(), json!("pending"));
    // Isolation (WM-INV-08), stronger than the baseline: a review of a
    // session-scoped memory copies its text, so the proposal stays in the
    // owning session instead of becoming project-visible.
    if target.scope == "session" {
        c.insert("scope".into(), json!("session"));
        if let Some(owner) = &target.owner_session_id {
            c.insert("ownerSessionId".into(), json!(owner));
        }
    } else {
        c.insert("scope".into(), json!(scope));
    }
    c.insert("confidence".into(), json!(confidence));
    c.insert("importance".into(), json!(importance));
    c.insert("reviewReason".into(), json!(reason));
    c.insert("suggestedAction".into(), json!(action));
    c.insert("tags".into(), json!(tags));
    c.insert("anchors".into(), anchors);
    c.insert("sources".into(), json!([{"type": "session"}]));
    c.insert("createdAt".into(), json!(now));
    c.insert("updatedAt".into(), json!(now));
    c
}

/// Results of the file-system part of verification (collected before the
/// write transaction so no IO happens while the writer lock is held).
struct VerifyPlan {
    outcomes: Vec<(String, bool)>,
    verified: Vec<String>,
    stale: Vec<String>,
    reactivations: Vec<(String, Vec<MemoryAnchor>)>,
}

fn plan_verification(
    root: Option<&Path>,
    active: &[SageRecord],
    stale_mem: &[SageRecord],
) -> Option<VerifyPlan> {
    let root = root?;
    let real_root = std::fs::canonicalize(root).ok()?;
    let mut existing: HashMap<String, bool> = HashMap::new();
    let mut paths: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for m in active {
        for a in &m.anchors {
            if let Some(p) = &a.path
                && is_path_anchor(&a.kind)
                && seen.insert(p.clone())
            {
                paths.push(p.clone());
            }
        }
    }
    for p in paths.iter().take(MAX_VERIFY_PATHS) {
        if let Some(real) = resolve_under(root, &real_root, p)
            && let Ok(meta) = std::fs::metadata(&real)
        {
            existing.insert(p.clone(), meta.is_dir());
        }
    }
    let mut plan = VerifyPlan {
        outcomes: vec![],
        verified: vec![],
        stale: vec![],
        reactivations: vec![],
    };
    for m in active {
        let all_valid = m.anchors.iter().all(|a| {
            let Some(p) = &a.path else { return true };
            if !is_path_anchor(&a.kind) {
                return true;
            }
            match existing.get(p) {
                None => false,
                Some(is_dir) if matches!(a.kind.as_str(), "directory" | "package") => *is_dir,
                Some(_) => true,
            }
        });
        plan.outcomes.push((m.id.clone(), all_valid));
        if all_valid {
            plan.verified.push(m.id.clone());
        } else {
            plan.stale.push(m.id.clone());
        }
    }
    for m in stale_mem {
        if existence_proves(&m.anchors) && anchors_present(root, &real_root, &m.anchors) {
            plan.reactivations.push((m.id.clone(), m.anchors.clone()));
        }
    }
    let failing: HashSet<String> = active
        .iter()
        .chain(stale_mem.iter())
        .filter(|m| validity_fails(root, &real_root, m))
        .map(|m| m.id.clone())
        .collect();
    for id in &failing {
        plan.reactivations.retain(|(r, _)| r != id);
        if let Some(o) = plan.outcomes.iter_mut().find(|(oid, ok)| oid == id && *ok) {
            o.1 = false;
            plan.verified.retain(|v| v != id);
            plan.stale.push(id.clone());
        }
    }
    Some(plan)
}

pub(crate) fn anchors_equal(a: &[MemoryAnchor], b: &[MemoryAnchor]) -> bool {
    let key = |x: &[MemoryAnchor]| -> HashSet<String> {
        x.iter()
            .map(|a| sorted_key(&serde_json::to_value(a).unwrap_or(Value::Null)))
            .collect()
    };
    key(a) == key(b)
}

impl SageStore {
    /// `hygiene`: one maintenance pass. See the module docs for the
    /// deliberate differences from the baseline.
    pub fn hygiene(
        &mut self,
        opts: &HygieneOptions,
        ctx: &MutationCtx,
    ) -> StoreResult<HygieneReport> {
        self.require_writable()?;
        let now = ctx.now_iso.clone();
        let now_ms = parse_iso_ms(&now)
            .ok_or_else(|| ErrorBody::new(ErrorCode::InvalidRequest, "invalid clock"))?;
        let root = self.project_root.clone();

        // File-system evidence first (no writer lock held during IO).
        let (active0, stale0) = {
            let all = all_memories(&self.conn)?;
            let active: Vec<SageRecord> = all
                .iter()
                .filter(|m| m.status == "active")
                .cloned()
                .collect();
            let stale: Vec<SageRecord> = all
                .into_iter()
                .filter(|m| {
                    m.status == "stale" && !m.anchors.is_empty() && is_verification_stale(m)
                })
                .collect();
            (active, stale)
        };
        let plan = if opts.verify != Some(false) {
            plan_verification(root.as_deref(), &active0, &stale0)
        } else {
            None
        };
        let examined = active0.len();
        let dry_run = opts.dry_run;

        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(sql_err)?;
        let result = run_pass(&self.conn, opts, plan, examined, &now, now_ms);
        match result {
            Ok(report) if !dry_run => {
                self.conn.execute_batch("COMMIT").map_err(sql_err)?;
                Ok(report)
            }
            Ok(mut report) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                report.dry_run = true;
                Ok(report)
            }
            Err(e) => {
                if !self.conn.is_autocommit() {
                    let _ = self.conn.execute_batch("ROLLBACK");
                }
                Err(e)
            }
        }
    }
}

fn run_pass(
    conn: &Connection,
    opts: &HygieneOptions,
    plan: Option<VerifyPlan>,
    examined: usize,
    now: &str,
    now_ms: i64,
) -> StoreResult<HygieneReport> {
    let mut report = HygieneReport {
        started_at: now.into(),
        completed_at: now.into(),
        examined,
        deduplicated: 0,
        superseded: 0,
        contradicted: 0,
        staled: 0,
        review_candidates_created: 0,
        archived: 0,
        archived_unused: 0,
        deleted: 0,
        purged_deleted: 0,
        verified: 0,
        reactivated: 0,
        transitive_merges: 0,
        dry_run: false,
    };

    // ── 1. Verification writes ────────────────────────────────────────────
    if let Some(plan) = plan {
        report.staled = plan.stale.len();
        report.verified = plan.verified.len();
        for (id, all_valid) in &plan.outcomes {
            let Some((_, cur)) = read_raw(conn, id).map_err(sql_err)? else {
                continue;
            };
            if cur.status != "active" || (*all_valid && cur.anchors.is_empty()) {
                continue;
            }
            if *all_valid {
                conn.execute(
                    "UPDATE memories SET data = json_set(data, '$.lastVerifiedAt', ?1, '$.freshness', 1), freshness = 1
                     WHERE id = ?2 AND json_valid(data)",
                    params![now, id],
                )
                .map_err(sql_err)?;
                continue;
            }
            if let Some(u) = semantic(
                conn,
                id,
                vec![
                    ("status", Some(json!("stale"))),
                    ("lastVerifiedAt", Some(json!(now))),
                    ("staleReason", Some(json!("verification"))),
                ],
                now,
            )? {
                sync_edges(conn, &u, now)?;
            }
        }
        for (id, observed) in &plan.reactivations {
            let Some((_, cur)) = read_raw(conn, id).map_err(sql_err)? else {
                continue;
            };
            if !is_verification_stale(&cur) || !anchors_equal(&cur.anchors, observed) {
                continue;
            }
            if let Some(u) = semantic(
                conn,
                id,
                vec![
                    ("status", Some(json!("active"))),
                    ("staleReason", None),
                    ("lastVerifiedAt", Some(json!(now))),
                    ("freshness", Some(json!(1))),
                ],
                now,
            )? {
                sync_edges(conn, &u, now)?;
                report.reactivated += 1;
            }
        }
    }

    // ── 2. Exact dedup ────────────────────────────────────────────────────
    {
        let mut groups: Vec<(String, Vec<SageRecord>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for m in list_status(conn, "active")? {
            if m.validity.is_some() {
                continue;
            }
            let key = format!(
                "{}\0{}\0{}",
                scope_key(&m),
                normalize_text_key(&m.text),
                audience_key(&m)
            );
            match index.get(&key) {
                Some(&i) => groups[i].1.push(m),
                None => {
                    index.insert(key.clone(), groups.len());
                    groups.push((key, vec![m]));
                }
            }
        }
        for (_, group) in groups {
            if group.len() < 2 {
                continue;
            }
            let mut sorted = group;
            sorted.sort_by(keeper_order);
            let keeper = sorted[0].clone();
            let dups: Vec<&SageRecord> = sorted[1..].iter().collect();
            let set: Vec<&SageRecord> = sorted.iter().collect();
            merge_group(conn, &keeper, &set, &dups, None, now)?;
            report.deduplicated += dups.len();
            report.superseded += dups.len();
        }
        audit(
            conn,
            "memory.hygiene_dedup",
            now,
            json!({"details": {"deduplicated": report.deduplicated, "superseded": report.superseded}}),
        )
        .map_err(sql_err)?;
    }

    // ── 3. Near-duplicate union-find ──────────────────────────────────────
    if opts.near_dedup != Some(false) {
        let mut buckets: Vec<Vec<SageRecord>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for m in list_status(conn, "active")? {
            if m.validity.is_some() {
                continue;
            }
            let key = format!("{}\0{}\0{}", scope_key(&m), m.kind, audience_key(&m));
            match index.get(&key) {
                Some(&i) => buckets[i].push(m),
                None => {
                    index.insert(key, buckets.len());
                    buckets.push(vec![m]);
                }
            }
        }
        let mut near = 0usize;
        for bucket in buckets {
            if bucket.len() < 2 {
                continue;
            }
            let mut capped = bucket;
            capped.sort_by(|a, b| {
                b.importance
                    .partial_cmp(&a.importance)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(
                        b.confidence
                            .partial_cmp(&a.confidence)
                            .unwrap_or(std::cmp::Ordering::Equal),
                    )
                    .then(a.created_at.cmp(&b.created_at))
            });
            capped.truncate(NEAR_DUP_BUCKET_CAP);
            let n = capped.len();
            let mut parent: Vec<usize> = (0..n).collect();
            fn find(p: &mut [usize], mut i: usize) -> usize {
                while p[i] != i {
                    p[i] = p[p[i]];
                    i = p[i];
                }
                i
            }
            for i in 0..n {
                for j in i + 1..n {
                    let (l, r) = (&capped[i], &capped[j]);
                    if is_near_duplicate(&l.text, &l.kind, &l.anchors, r)
                        && !is_possibly_contradictory(&l.text, &r.text)
                    {
                        let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                        if ri != rj {
                            parent[rj] = ri;
                        }
                    }
                }
            }
            let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for i in 0..n {
                let root = find(&mut parent, i);
                groups.entry(root).or_default().push(i);
            }
            for members in groups.values() {
                if members.len() < 2 {
                    continue;
                }
                if members.len() > 2 {
                    report.transitive_merges += 1;
                }
                let mut sorted: Vec<&SageRecord> = members.iter().map(|&i| &capped[i]).collect();
                sorted.sort_by(|a, b| keeper_order(a, b));
                let keeper = sorted[0];
                let dups: Vec<&SageRecord> = sorted[1..]
                    .iter()
                    .copied()
                    .filter(|d| is_near_duplicate(&keeper.text, &keeper.kind, &keeper.anchors, d))
                    .collect();
                if dups.is_empty() {
                    continue;
                }
                let mut set = vec![keeper];
                set.extend(dups.iter().copied());
                let richest = set.iter().copied().fold(keeper, |best, cur| {
                    if js_length(&cur.text) > js_length(&best.text) {
                        cur
                    } else {
                        best
                    }
                });
                let text = (js_length(&richest.text) > js_length(&keeper.text))
                    .then_some(richest.text.as_str());
                merge_group(
                    conn,
                    keeper,
                    &set,
                    &dups,
                    Some(text.unwrap_or(&keeper.text)),
                    now,
                )?;
                report.deduplicated += dups.len();
                report.superseded += dups.len();
                near += dups.len();
            }
        }
        if near > 0 || report.transitive_merges > 0 {
            audit(
                conn,
                "memory.hygiene_near_dedup",
                now,
                json!({"details": {"nearPassDeduped": near, "transitiveMerges": report.transitive_merges,
                        "deduplicated": report.deduplicated, "superseded": report.superseded}}),
            )
            .map_err(sql_err)?;
        }
    }

    // ── 4. Contradictions ─────────────────────────────────────────────────
    {
        let mut buckets: Vec<Vec<SageRecord>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for m in list_status(conn, "active")? {
            if m.validity.is_some() {
                continue;
            }
            let key = format!("{}\u{0}{}", scope_key(&m), audience_key(&m));
            match index.get(&key) {
                Some(&i) => buckets[i].push(m),
                None => {
                    index.insert(key, buckets.len());
                    buckets.push(vec![m]);
                }
            }
        }
        let mut flagged: Vec<(SageRecord, SageRecord)> = Vec::new();
        let mut flagged_ids = HashSet::new();
        for bucket in &buckets {
            if bucket.len() < 2 {
                continue;
            }
            let capped = &bucket[..bucket.len().min(NEAR_DUP_BUCKET_CAP)];
            for i in 0..capped.len() {
                for j in i + 1..capped.len() {
                    let (a, b) = (&capped[i], &capped[j]);
                    if !is_possibly_contradictory(&a.text, &b.text) {
                        continue;
                    }
                    let has = |x: &SageRecord, y: &SageRecord| {
                        x.contradicts.as_ref().is_some_and(|c| c.contains(&y.id))
                    };
                    if has(a, b) || has(b, a) {
                        continue;
                    }
                    let a_older = (a.created_at.as_str(), a.id.as_str())
                        <= (b.created_at.as_str(), b.id.as_str());
                    let (newer, other) = if a_older { (b, a) } else { (a, b) };
                    if flagged_ids.insert(newer.id.clone()) {
                        flagged.push((newer.clone(), other.clone()));
                    }
                }
            }
        }
        let mut pending: Vec<(SageRecord, SageRecord)> = Vec::new();
        for (memory, other) in flagged {
            let Some((_, cur)) = read_raw(conn, &memory.id).map_err(sql_err)? else {
                continue;
            };
            let Some((_, cur_other)) = read_raw(conn, &other.id).map_err(sql_err)? else {
                continue;
            };
            if cur.status != "active" || cur_other.status != "active" {
                continue;
            }
            if cur.validity.is_some() || cur_other.validity.is_some() {
                continue;
            }
            if format!("{}\u{0}{}", scope_key(&cur), audience_key(&cur))
                != format!("{}\u{0}{}", scope_key(&cur_other), audience_key(&cur_other))
            {
                continue;
            }
            if !is_possibly_contradictory(&cur.text, &cur_other.text) {
                continue;
            }
            let has = |x: &SageRecord, y: &SageRecord| {
                x.contradicts.as_ref().is_some_and(|c| c.contains(&y.id))
            };
            if has(&cur, &cur_other) || has(&cur_other, &cur) {
                continue;
            }
            let mut contradicts = cur.contradicts.clone().unwrap_or_default();
            contradicts.push(cur_other.id.clone());
            if let Some(u) = semantic(
                conn,
                &cur.id,
                vec![("contradicts", Some(json!(contradicts)))],
                now,
            )? {
                sync_edges(conn, &u, now)?;
                pending.push((u, cur_other));
                report.contradicted += 1;
            }
        }
        if report.contradicted > 0 {
            audit(
                conn,
                "memory.hygiene_contradictions",
                now,
                json!({"details": {"candidates": report.contradicted}}),
            )
            .map_err(sql_err)?;
        }
        for (memory, other) in pending {
            let anchors = raw_array(conn, &memory.id, "anchors")?;
            let c = review_candidate(
                ulid::Ulid::generate().to_string(),
                &memory,
                &memory.scope,
                memory.confidence,
                memory.importance,
                &format!("Possible contradiction with memory {}", other.id),
                "investigate",
                memory.tags.clone(),
                Value::Array(anchors),
                now,
            );
            add_candidate(conn, c)?;
        }
    }

    // ── 5. Review candidates and session GC ──────────────────────────────
    let retention_ms = opts.retention_days.unwrap_or(90.0) * DAY_MS;
    let session_days = opts
        .session_retention_days
        .unwrap_or(DEFAULT_SESSION_RETENTION_DAYS);
    let session_ms = if session_days.is_finite() && session_days > 0.0 {
        session_days * DAY_MS
    } else {
        f64::INFINITY
    };
    let low_conf_ms = opts.archive_low_confidence_after_days.unwrap_or(30.0) * DAY_MS;
    let unused_ms = opts.archive_unused_after_days.unwrap_or(30.0) * DAY_MS;
    let unused_min = opts.unused_min_injections.unwrap_or(10.0).floor().max(1.0);
    let candidates_all: Vec<Value> = conn
        .prepare("SELECT data FROM candidates ORDER BY updated_at DESC")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(sql_err)?
        .iter()
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();
    let pending_targets: HashSet<String> = candidates_all
        .iter()
        .filter(|c| c["status"] == "pending")
        .map(|c| c["targetMemoryId"].as_str().unwrap_or("").to_string())
        .collect();
    let mut reviewed: HashMap<String, Vec<String>> = HashMap::new();
    for c in &candidates_all {
        if c["status"] == "pending" || c["kind"] != "memory_review" {
            continue;
        }
        let Some(target) = c["targetMemoryId"].as_str().filter(|t| !t.is_empty()) else {
            continue;
        };
        let Some(at) = ms(c["updatedAt"].as_str()) else {
            continue;
        };
        let age = now_ms - at;
        if !(0..=REVIEW_SUPPRESSION_MS).contains(&age) {
            continue;
        }
        let text = normalize_text(c["text"].as_str().unwrap_or(""));
        if text.is_empty() {
            continue;
        }
        reviewed.entry(target.to_string()).or_default().push(text);
    }
    let was_reviewed = |m: &SageRecord| {
        reviewed.get(&m.id).is_some_and(|texts| {
            texts
                .iter()
                .any(|t| normalize_text(&m.text).starts_with(t.as_str()))
        })
    };
    let expired = |m: &SageRecord| -> bool {
        m.scope == "session"
            && match opt_stamp(&m.expires_at) {
                Some(e) => ms(Some(e)).is_some_and(|e| e <= now_ms),
                None => ms(Some(&m.updated_at)).is_some_and(|u| (now_ms - u) as f64 >= session_ms),
            }
    };
    let mut gc_targets: Vec<String> = Vec::new();
    for m in all_memories(conn)? {
        if matches!(m.status.as_str(), "deleted" | "superseded" | "contradicted")
            || is_permanent(&m)
        {
            continue;
        }
        if expired(&m) {
            gc_targets.push(m.id.clone());
            continue;
        }
        let age =
            ms(opt_stamp(&m.last_accessed_at).or(Some(&m.updated_at))).map(|t| (now_ms - t) as f64);
        let aged = |limit: f64| age.is_some_and(|a| a >= limit);
        let persistence = m.persistence.clone().unwrap_or_else(|| "long_lived".into());
        let (reason, action) = if opt_stamp(&m.expires_at)
            .and_then(|e| ms(Some(e)))
            .is_some_and(|e| e <= now_ms)
        {
            (Some("expires_at_passed"), "delete")
        } else if m.status == "active"
            && m.scope != "session"
            && m.audience.is_none()
            && m.injection_count.unwrap_or(0) as f64 >= unused_min
            && m.use_count.unwrap_or(0) == 0
            && aged(unused_ms)
        {
            (
                Some("injected_never_used"),
                if m.importance >= CRITICAL_IMPORTANCE_FLOOR {
                    "investigate"
                } else {
                    "delete"
                },
            )
        } else if (m.status == "stale" && aged(retention_ms))
            || (m.confidence < 0.5 && aged(low_conf_ms))
        {
            (
                Some(if m.confidence < 0.5 {
                    "confidence_low"
                } else {
                    "freshness_low"
                }),
                "investigate",
            )
        } else {
            (None, "investigate")
        };
        let Some(reason) = reason else { continue };
        if pending_targets.contains(&m.id) || was_reviewed(&m) {
            continue;
        }
        let age_days =
            ms(Some(&m.updated_at)).map(|u| ((now_ms - u) as f64 / DAY_MS).floor() as i64);
        let mut tags = m.tags.clone();
        tags.push(format!("persistence:{persistence}"));
        let anchors = raw_array(conn, &m.id, "anchors")?;
        let c = review_candidate(
            ulid::Ulid::generate().to_string(),
            &m,
            "project",
            0.6,
            0.4,
            reason,
            action,
            tags,
            Value::Array(anchors),
            now,
        );
        if !add_candidate(conn, c)? {
            continue;
        }
        audit(
            conn,
            "memory.review_candidate_created",
            now,
            json!({"memoryId": m.id, "reason": reason, "details": {
                "suggestedAction": action, "ageDays": age_days, "persistence": persistence,
                "status": m.status, "confidence": m.confidence,
            }}),
        )
        .map_err(sql_err)?;
        report.review_candidates_created += 1;
    }
    let mut tombstoned = HashSet::new();
    for id in gc_targets {
        let Some((mut raw, cur)) = read_raw(conn, &id).map_err(sql_err)? else {
            continue;
        };
        if matches!(
            cur.status.as_str(),
            "deleted" | "superseded" | "contradicted"
        ) || is_permanent(&cur)
            || !expired(&cur)
        {
            continue;
        }
        raw.insert("status".into(), json!("deleted"));
        raw.insert("revision".into(), json!(cur.revision + 1));
        raw.insert("updatedAt".into(), json!(now));
        raw.insert("contextPolicy".into(), json!("never"));
        upsert_raw(conn, &mut raw)?;
        tombstoned.insert(id.clone());
        cascade_delete_edges(conn, &format!("mem:{id}"))?;
        let mut details = Map::new();
        if let Some(e) = &cur.expires_at {
            details.insert("expiresAt".into(), json!(e));
        }
        details.insert("updatedAt".into(), json!(cur.updated_at));
        details.insert("sessionRetentionDays".into(), json!(session_days));
        audit(
            conn,
            "memory.session_gc",
            now,
            json!({"memoryId": id,
                   "reason": if cur.expires_at.is_some() { "expires_at_passed" } else { "session_retention" },
                   "details": details}),
        )
        .map_err(sql_err)?;
        report.deleted += 1;
    }
    clean_referencing(conn, &tombstoned, now)?;

    // ── 6. Optional purge of old tombstones ──────────────────────────────
    if let Some(days) = opts.purge_deleted_after_days.filter(|d| *d > 0.0) {
        let cutoff = now_ms as f64 - days * DAY_MS;
        let ids: Vec<String> = list_status(conn, "deleted")?
            .into_iter()
            .filter(|m| {
                !is_permanent(m) && ms(Some(&m.updated_at)).is_some_and(|u| (u as f64) <= cutoff)
            })
            .map(|m| m.id)
            .collect();
        if !ids.is_empty() {
            for id in &ids {
                let Some((_, cur)) = read_raw(conn, id).map_err(sql_err)? else {
                    continue;
                };
                if cur.status != "deleted" || is_permanent(&cur) {
                    continue;
                }
                cascade_delete_edges(conn, &format!("mem:{id}"))?;
                conn.execute("DELETE FROM memories WHERE id = ?1", [id])
                    .map_err(sql_err)?;
                report.purged_deleted += 1;
            }
            audit(
                conn,
                "memory.purge_deleted",
                now,
                json!({"details": {"purgedDeleted": report.purged_deleted, "purgeDeletedAfterDays": days}}),
            )
            .map_err(sql_err)?;
        }
    }

    report.completed_at = now.into();
    let mut details = serde_json::to_value(&report).unwrap_or(Value::Null);
    if let Some(m) = details.as_object_mut() {
        m.remove("dryRun");
    }
    audit(
        conn,
        "memory.hygiene_completed",
        now,
        json!({"details": details}),
    )
    .map_err(sql_err)?;
    // `pruneAuditLog`: keep the newest 1000 rows.
    conn.execute(
        "DELETE FROM audit_log WHERE id NOT IN (SELECT id FROM audit_log ORDER BY id DESC LIMIT 1000)",
        [],
    )
    .map_err(sql_err)?;
    Ok(report)
}

#[cfg(test)]
mod path_guard_tests {
    use super::*;

    #[test]
    fn stored_anchor_paths_outside_the_root_are_never_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "x").unwrap();
        std::fs::write(tmp.path().join("outside.rs"), "x").unwrap();
        let real_root = std::fs::canonicalize(&root).unwrap();
        assert!(resolve_under(&root, &real_root, "src/a.rs").is_some());
        assert!(resolve_under(&root, &real_root, "../outside.rs").is_none());
        let abs = tmp.path().join("outside.rs");
        assert!(resolve_under(&root, &real_root, &abs.to_string_lossy()).is_none());
        assert!(resolve_under(&root, &real_root, r"\attacker\share\a.rs").is_none());
        assert!(resolve_under(&root, &real_root, "//attacker/share/a.rs").is_none());
    }
}
