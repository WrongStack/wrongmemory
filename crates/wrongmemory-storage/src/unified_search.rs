//! `unifiedSearchService` (baseline `sqlite-store-search.ts`,
//! `sqlite-search-cursor.ts`, `sqlite-store-search-adjacent.ts`).
//!
//! Filters (kinds, scopes, importance, freshness, paths), audience and anchor
//! post-filters (5× over-fetch), four rankings with a strict total order,
//! keyset cursors bound to the query/filters/ranking/session, absolute
//! scores, and lexical-adjacency suggestions.
//!
//! The session context is the authenticated connection's session; the
//! baseline's `includeAllSessions` administrative bypass does not exist here.

use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use wrongmemory_contracts::{ErrorBody, ErrorCode, MemoryAnchor, MemoryAudience, SageRecord};

use crate::paths::normalize_project_path;
use crate::retrieval::{base64url_decode, base64url_encode, escape_glob};
use crate::store::{SageStore, StoreResult, Viewer, decode_memory, fts_prefix_terms, sql_err};
use crate::text::{normalize_slashes, parse_iso_ms};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: usize = 200;
const JS_FILTER_OVERFETCH: usize = 5;
const RECENCY_WINDOW_MS: f64 = 90.0 * 24.0 * 60.0 * 60.0 * 1000.0;
const SEARCH_CURSOR_VERSION: i64 = 1;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Freshness {
    pub created_after: Option<String>,
    pub verified_after: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchQuery {
    pub text: Option<String>,
    pub kinds: Option<Vec<String>>,
    pub scopes: Option<Vec<String>>,
    pub importance_at_least: Option<f64>,
    pub freshness: Option<Freshness>,
    pub paths: Option<Vec<String>>,
    pub audience: Option<MemoryAudience>,
    pub anchor: Option<MemoryAnchor>,
    pub cursor: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchOptions {
    pub limit: Option<f64>,
    pub include_statuses: Option<Vec<String>>,
    pub ranking: Option<String>,
    pub suggest: Option<String>,
    pub exclude_session_scoped: Option<bool>,
}

type Sql = rusqlite::types::Value;

fn cursor_error(why: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::InvalidRequest,
        format!("Invalid search cursor ({why}) — restart the search from the first page."),
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Key {
    Bm25,
    Importance,
    UpdatedAt,
    Id,
}

fn sort_keys(ranking: &str, has_bm25: bool) -> Vec<Key> {
    match ranking {
        "recency" => vec![Key::UpdatedAt, Key::Importance, Key::Id],
        "importance" => vec![Key::Importance, Key::UpdatedAt, Key::Id],
        _ if has_bm25 => vec![Key::Bm25, Key::Importance, Key::UpdatedAt, Key::Id],
        _ => vec![Key::Importance, Key::UpdatedAt, Key::Id],
    }
}

fn key_expr(k: Key, fts: bool) -> &'static str {
    match (k, fts) {
        (Key::Bm25, _) => "bm25(memories_fts)",
        (Key::Importance, true) => "m.importance",
        (Key::Importance, false) => "importance",
        (Key::UpdatedAt, true) => "m.updated_at",
        (Key::UpdatedAt, false) => "updated_at",
        (Key::Id, true) => "m.id",
        (Key::Id, false) => "id",
    }
}

fn order_by(ranking: &str, fts: bool) -> String {
    sort_keys(ranking, fts)
        .into_iter()
        .map(|k| {
            format!(
                "{} {}",
                key_expr(k, fts),
                if k == Key::Bm25 { "ASC" } else { "DESC" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone)]
struct CursorValues {
    bm25: Option<f64>,
    importance: f64,
    updated_at: String,
    id: String,
}

impl CursorValues {
    fn get(&self, k: Key) -> Option<Sql> {
        match k {
            Key::Bm25 => self.bm25.map(Sql::Real),
            Key::Importance => Some(Sql::Real(self.importance)),
            Key::UpdatedAt => Some(Sql::Text(self.updated_at.clone())),
            Key::Id => Some(Sql::Text(self.id.clone())),
        }
    }
}

fn keyset(keys: &[Key], c: &CursorValues, fts: bool) -> StoreResult<(String, Vec<Sql>)> {
    let mut parts = Vec::new();
    let mut params = Vec::new();
    for i in 0..keys.len() {
        let mut conds = Vec::new();
        for &k in &keys[..i] {
            params.push(c.get(k).ok_or_else(|| cursor_error("missing sort key"))?);
            conds.push(format!("{} = ?", key_expr(k, fts)));
        }
        let b = keys[i];
        params.push(c.get(b).ok_or_else(|| cursor_error("missing sort key"))?);
        conds.push(format!(
            "{} {} ?",
            key_expr(b, fts),
            if b == Key::Bm25 { ">" } else { "<" }
        ));
        parts.push(conds.join(" AND "));
    }
    Ok((format!("({})", parts.join(" OR ")), params))
}

fn sorted(v: &Option<Vec<String>>) -> Vec<String> {
    let mut v = v.clone().unwrap_or_default();
    v.sort();
    v
}

/// `computeCursorBinding`. Key order matches the baseline's literal.
fn binding(
    q: &SearchQuery,
    raw_query: &Value,
    statuses: &[String],
    ranking: &str,
    session: &Option<String>,
    exclude: bool,
) -> String {
    let mut st = statuses.to_vec();
    st.sort();
    let canonical = json!({
        "text": q.text.as_deref().map(str::trim).unwrap_or(""),
        "kinds": sorted(&q.kinds),
        "scopes": sorted(&q.scopes),
        "importanceAtLeast": q.importance_at_least,
        "freshnessCreated": q.freshness.as_ref().and_then(|f| f.created_after.clone()),
        "freshnessVerified": q.freshness.as_ref().and_then(|f| f.verified_after.clone()),
        "audience": raw_query.get("audience").cloned().unwrap_or(Value::Null),
        "anchor": raw_query.get("anchor").cloned().unwrap_or(Value::Null),
        "paths": sorted(&q.paths),
        "statuses": st,
        "ranking": ranking,
        "sessionId": session,
        "includeAllSessions": false,
        "excludeSessionScoped": exclude,
    });
    let digest = Sha256::digest(canonical.to_string().as_bytes());
    base64url_encode(&digest)[..22].to_string()
}

fn encode_cursor(binding: &str, ranking: &str, channel: &str, v: &CursorValues) -> String {
    let mut p = Map::new();
    p.insert("v".into(), json!(SEARCH_CURSOR_VERSION));
    p.insert("h".into(), json!(binding));
    p.insert("r".into(), json!(ranking));
    p.insert("c".into(), json!(channel));
    p.insert("p".into(), json!(v.importance));
    p.insert("u".into(), json!(v.updated_at));
    p.insert("i".into(), json!(v.id));
    if let Some(b) = v.bm25 {
        p.insert("b".into(), json!(b));
    }
    base64url_encode(Value::Object(p).to_string().as_bytes())
}

fn decode_cursor(
    token: &str,
    binding: &str,
    ranking: &str,
    channel: &str,
) -> StoreResult<CursorValues> {
    let parsed: Value = base64url_decode(token)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| cursor_error("malformed token"))?;
    let o = parsed
        .as_object()
        .ok_or_else(|| cursor_error("malformed token"))?;
    if o.get("v").and_then(Value::as_i64) != Some(SEARCH_CURSOR_VERSION) {
        return Err(cursor_error("unsupported version"));
    }
    if o.get("h").and_then(Value::as_str) != Some(binding) {
        return Err(cursor_error("query/filter binding mismatch"));
    }
    if o.get("r").and_then(Value::as_str) != Some(ranking) {
        return Err(cursor_error("ranking mismatch"));
    }
    if o.get("c").and_then(Value::as_str) != Some(channel) {
        return Err(cursor_error("channel mismatch"));
    }
    let id = o
        .get("i")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| cursor_error("missing id"))?;
    let u = o
        .get("u")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| cursor_error("missing updatedAt"))?;
    let p = o
        .get("p")
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite())
        .ok_or_else(|| cursor_error("missing importance"))?;
    let needs_bm25 = sort_keys(ranking, channel == "fts").contains(&Key::Bm25);
    let b = o.get("b");
    let bm25 = if needs_bm25 {
        Some(
            b.and_then(Value::as_f64)
                .filter(|f| f.is_finite())
                .ok_or_else(|| cursor_error("missing bm25"))?,
        )
    } else {
        if b.is_some() {
            return Err(cursor_error("unexpected bm25"));
        }
        None
    };
    Ok(CursorValues {
        bm25,
        importance: p,
        updated_at: u.to_string(),
        id: id.to_string(),
    })
}

fn norm_values(v: &Option<Vec<String>>) -> Option<Vec<String>> {
    v.as_ref().map(|l| {
        l.iter()
            .map(|s| s.nfkc().collect::<String>().trim().to_lowercase())
            .collect()
    })
}

/// `audienceSelectorMatches`.
fn audience_matches(want: &MemoryAudience, have: Option<&MemoryAudience>) -> bool {
    let Some(have) = have else { return false };
    let w = (
        norm_values(&want.roles),
        norm_values(&want.task_types),
        norm_values(&want.modes),
    );
    let h = (
        norm_values(&have.roles),
        norm_values(&have.task_types),
        norm_values(&have.modes),
    );
    let intersects = |a: &Option<Vec<String>>, b: &Vec<String>| {
        a.as_ref().is_some_and(|a| a.iter().any(|x| b.contains(x)))
    };
    if let Some(r) = &h.0
        && !intersects(&w.0, r)
    {
        return false;
    }
    if let Some(t) = &h.1
        && !intersects(&w.1, t)
    {
        return false;
    }
    if let Some(m) = &h.2
        && !intersects(&w.2, m)
    {
        return false;
    }
    true
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `anchorMatches`.
fn anchor_matches(
    want: &MemoryAnchor,
    anchors: &[MemoryAnchor],
    root: Option<&std::path::Path>,
) -> bool {
    let wanted_path = match &want.path {
        Some(p) if !p.is_empty() => {
            let Some(root) = root else { return false };
            match normalize_project_path(root, p) {
                Ok(n) => Some(n.to_lowercase()),
                Err(_) => return false,
            }
        }
        _ => None,
    };
    let wanted_symbol = want.symbol.as_deref().map(str::trim);
    let wanted_command = want.command.as_deref().map(|c| collapse_ws(c.trim()));
    let wanted_role = want.role.as_deref().map(|r| r.trim().to_lowercase());
    anchors.iter().any(|a| {
        if a.kind != want.kind {
            return false;
        }
        if let Some(wp) = &wanted_path
            && a.path
                .as_deref()
                .map(|p| normalize_slashes(p).to_lowercase())
                .as_ref()
                != Some(wp)
        {
            return false;
        }
        if let Some(ws) = wanted_symbol
            && a.symbol.as_deref().map(str::trim).unwrap_or("") != ws
        {
            return false;
        }
        if let Some(wc) = &wanted_command
            && a.command
                .as_deref()
                .map(|c| collapse_ws(c.trim()))
                .unwrap_or_default()
                != *wc
        {
            return false;
        }
        if let Some(wr) = &wanted_role
            && a.role
                .as_deref()
                .map(|r| r.trim().to_lowercase())
                .unwrap_or_default()
                != *wr
        {
            return false;
        }
        true
    })
}

/// `computeAbsoluteScores`.
fn scores(memories: &[SageRecord], bm25: &[Option<f64>], now_ms: i64) -> Vec<f64> {
    memories
        .iter()
        .zip(bm25)
        .map(|(m, rank)| {
            let metadata = m.importance * 0.5 + m.confidence * 0.3 + m.freshness * 0.2;
            match rank {
                Some(r) => (1.0 / (1.0 + r.exp())) * (0.75 + 0.25 * metadata),
                None => {
                    let recency = parse_iso_ms(&m.updated_at)
                        .map(|t| (1.0 - (now_ms - t) as f64 / RECENCY_WINDOW_MS).clamp(0.0, 1.0))
                        .unwrap_or(0.0);
                    0.6 * recency + 0.4 * metadata
                }
            }
        })
        .collect()
}

fn hit(m: &SageRecord, score: f64, bm25: Option<f64>, reason: &str) -> Value {
    let mut h = json!({
        "id": m.id,
        "kind": m.kind,
        "scope": m.scope,
        "tags": m.tags,
        "anchors": m.anchors,
        "text": m.text,
        "importance": m.importance,
        "confidence": m.confidence,
        "status": m.status,
        "createdAt": m.created_at,
        "updatedAt": m.updated_at,
        "score": score,
        "bm25": bm25,
        "matchReason": reason,
    });
    if let Some(a) = &m.audience {
        h["audience"] = json!(a);
    }
    if let Some(v) = &m.last_verified_at {
        h["verifiedAt"] = json!(v);
    }
    h
}

/// Shared WHERE filters (kinds, scopes, importance, freshness, paths,
/// session, session-scope exclusion) for a table prefix.
struct Filters<'a> {
    q: &'a SearchQuery,
    path_targets: Option<(Vec<String>, Vec<String>)>,
    session: &'a Option<String>,
    exclude_session_scoped: bool,
}

impl Filters<'_> {
    fn push(&self, prefix: &str, clauses: &mut Vec<String>, params: &mut Vec<Sql>) {
        let q = self.q;
        let list = |clauses: &mut Vec<String>,
                    params: &mut Vec<Sql>,
                    col: &str,
                    v: &Option<Vec<String>>| {
            if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
                clauses.push(format!(
                    "{prefix}{col} IN ({})",
                    vec!["?"; v.len()].join(",")
                ));
                params.extend(v.iter().cloned().map(Sql::Text));
            }
        };
        list(clauses, params, "kind", &q.kinds);
        list(clauses, params, "scope", &q.scopes);
        if let Some(i) = q.importance_at_least {
            clauses.push(format!("{prefix}importance >= ?"));
            params.push(Sql::Real(i));
        }
        if let Some(f) = &q.freshness {
            if let Some(c) = &f.created_after {
                clauses.push(format!("{prefix}created_at >= ?"));
                params.push(Sql::Text(c.clone()));
            }
            if let Some(v) = &f.verified_after {
                clauses.push(format!(
                    "CASE WHEN json_valid({prefix}data) THEN json_extract({prefix}data, '$.lastVerifiedAt') END >= ?"
                ));
                params.push(Sql::Text(v.clone()));
            }
        }
        if let Some((targets, globs)) = &self.path_targets {
            let mut conds = Vec::new();
            if !targets.is_empty() {
                conds.push(format!(
                    "e.to_node IN ({})",
                    vec!["?"; targets.len()].join(",")
                ));
            }
            if !globs.is_empty() {
                conds.push(vec!["e.to_node GLOB ?"; globs.len()].join(" OR "));
            }
            if conds.is_empty() {
                conds.push("0".into());
            }
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM edges e WHERE e.from_node = 'mem:' || {prefix}id AND ({}))",
                conds.join(" OR ")
            ));
            params.extend(targets.iter().cloned().map(Sql::Text));
            params.extend(globs.iter().cloned().map(Sql::Text));
        }
        match self.session {
            Some(s) => {
                clauses.push(format!(
                    "({prefix}scope != 'session' OR {prefix}owner_session_id = ?)"
                ));
                params.push(Sql::Text(s.clone()));
            }
            None => clauses.push(format!(
                "({prefix}scope != 'session' OR {prefix}owner_session_id IS NULL)"
            )),
        }
        if self.exclude_session_scoped {
            clauses.push(format!("{prefix}scope != ?"));
            params.push(Sql::Text("session".into()));
        }
    }
}

impl SageStore {
    fn rows_with_bm25(
        &self,
        sql: &str,
        params: Vec<Sql>,
        fts: bool,
    ) -> StoreResult<Vec<(String, Option<f64>)>> {
        let mut stmt = self.conn.prepare(sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    if fts {
                        r.get::<_, Option<f64>>(1)?
                    } else {
                        None
                    },
                ))
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(rows)
    }

    /// `executeUnifiedSearch`. `raw_query` is the caller's query object (its
    /// `audience`/`anchor` are bound into the cursor as given).
    pub fn unified_search(
        &self,
        raw_query: &Value,
        raw_options: &Value,
        viewer: &Viewer,
        now_iso: &str,
    ) -> StoreResult<Value> {
        let bad = |e: serde_json::Error| {
            ErrorBody::new(
                ErrorCode::InvalidRequest,
                format!("invalid search query: {e}"),
            )
        };
        let q: SearchQuery = serde_json::from_value(raw_query.clone()).map_err(bad)?;
        let o: SearchOptions = serde_json::from_value(raw_options.clone()).map_err(bad)?;
        let limit =
            (o.limit.map_or(DEFAULT_LIMIT as f64, f64::floor).max(1.0) as usize).min(MAX_LIMIT);
        let statuses = o
            .include_statuses
            .clone()
            .unwrap_or_else(|| vec!["active".into()]);
        if statuses
            .iter()
            .any(|s| !wrongmemory_contracts::SAGE_STATUSES.contains(&s.as_str()))
        {
            return Err(ErrorBody::new(ErrorCode::InvalidRequest, "unknown status"));
        }
        let ranking = o.ranking.clone().unwrap_or_else(|| "hybrid".into());
        if !matches!(
            ranking.as_str(),
            "relevance" | "recency" | "importance" | "hybrid"
        ) {
            return Err(ErrorBody::new(ErrorCode::InvalidRequest, "unknown ranking"));
        }
        let exclude = o.exclude_session_scoped == Some(true);
        let cursor = match &q.cursor {
            None => None,
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(_) => {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidRequest,
                    "Search cursor must be a non-empty opaque token from SearchResult.nextCursor.",
                ));
            }
        };

        let text = q.text.as_deref().map(str::trim).unwrap_or("");
        let terms = if text.is_empty() {
            vec![]
        } else {
            fts_prefix_terms(text)
        };
        let match_expr = (!terms.is_empty()).then(|| terms.join(" AND "));
        let fts = match_expr.is_some();
        let channel = if fts { "fts" } else { "plain" };
        let bind = binding(
            &q,
            raw_query,
            &statuses,
            &ranking,
            &viewer.session_id,
            exclude,
        );
        let cursor_values = match &cursor {
            Some(c) => Some(decode_cursor(c, &bind, &ranking, channel)?),
            None => None,
        };
        if statuses.is_empty() {
            return Ok(json!({
                "hits": [], "suggestions": [], "totalCandidates": 0, "rankingApplied": ranking,
                "queryEcho": {}, "nextCursor": null, "matchChannel": channel,
            }));
        }
        let root = self.project_root.clone();
        let path_targets = q.paths.as_ref().filter(|p| !p.is_empty()).map(|paths| {
            let mut targets: Vec<String> = Vec::new();
            let mut globs = Vec::new();
            let add = |t: String, targets: &mut Vec<String>| {
                if !targets.contains(&t) {
                    targets.push(t);
                }
            };
            for p in paths {
                let Some(n) = root
                    .as_deref()
                    .and_then(|r| normalize_project_path(r, p).ok())
                else {
                    continue;
                };
                add(format!("file:{n}"), &mut targets);
                globs.push(format!("symbol:{}#*", escape_glob(&n)));
                let parts: Vec<&str> = n.split('/').filter(|x| !x.is_empty()).collect();
                for i in (1..=parts.len()).rev() {
                    let a = parts[..i].join("/");
                    add(format!("file:{a}"), &mut targets);
                    add(format!("dir:{a}"), &mut targets);
                }
            }
            (targets, globs)
        });
        let filters = Filters {
            q: &q,
            path_targets,
            session: &viewer.session_id,
            exclude_session_scoped: exclude,
        };
        let js_filters = q.audience.is_some() || q.anchor.is_some();
        let keep = |m: &SageRecord| -> bool {
            q.audience
                .as_ref()
                .is_none_or(|a| audience_matches(a, m.audience.as_ref()))
                && q.anchor
                    .as_ref()
                    .is_none_or(|a| anchor_matches(a, &m.anchors, root.as_deref()))
        };
        let sql_limit = if js_filters {
            (limit * JS_FILTER_OVERFETCH).min(MAX_LIMIT * 5)
        } else {
            limit
        };

        let status_ph = vec!["?"; statuses.len()].join(",");
        let (data_rows, total, reason): (Vec<(String, Option<f64>)>, i64, &str) = if let Some(
            expr,
        ) = &match_expr
        {
            let mut clauses = vec![
                format!("m.status IN ({status_ph})"),
                "memories_fts MATCH ?".into(),
            ];
            let mut params: Vec<Sql> = statuses.iter().cloned().map(Sql::Text).collect();
            params.push(Sql::Text(expr.clone()));
            filters.push("m.", &mut clauses, &mut params);
            let base_where = format!(" WHERE {}", clauses.join(" AND "));
            let (mut data_where, mut data_params) = (base_where.clone(), params.clone());
            if let Some(c) = &cursor_values {
                let (k, kp) = keyset(&sort_keys(&ranking, true), c, true)?;
                data_where = format!(" WHERE {} AND {k}", clauses.join(" AND "));
                data_params.extend(kp);
            }
            let rows = self.rows_with_bm25(
                &format!(
                    "SELECT m.data, bm25(memories_fts) FROM memories_fts f CROSS JOIN memories m ON m.rowid = f.rowid{data_where} ORDER BY {} LIMIT {}",
                    order_by(&ranking, true),
                    sql_limit + 1
                ),
                data_params,
                true,
            )?;
            let total: i64 = self
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM memories_fts f CROSS JOIN memories m ON m.rowid = f.rowid{base_where}"),
                    rusqlite::params_from_iter(params),
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            (rows, total, "lexical")
        } else {
            let mut clauses = vec![format!("status IN ({status_ph})")];
            let mut params: Vec<Sql> = statuses.iter().cloned().map(Sql::Text).collect();
            filters.push("", &mut clauses, &mut params);
            let base_where = format!(" WHERE {}", clauses.join(" AND "));
            let (mut data_where, mut data_params) = (base_where.clone(), params.clone());
            if let Some(c) = &cursor_values {
                let (k, kp) = keyset(&sort_keys(&ranking, false), c, false)?;
                data_where = format!(" WHERE {} AND {k}", clauses.join(" AND "));
                data_params.extend(kp);
            }
            let rows = self.rows_with_bm25(
                &format!(
                    "SELECT data FROM memories{data_where} ORDER BY {} LIMIT {}",
                    order_by(&ranking, false),
                    sql_limit + 1
                ),
                data_params,
                false,
            )?;
            let total: i64 = self
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM memories{base_where}"),
                    rusqlite::params_from_iter(params),
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            (rows, total, "recency")
        };

        let decoded: Vec<(SageRecord, Option<f64>)> = data_rows
            .iter()
            .filter_map(|(d, b)| decode_memory(d).map(|m| (m, *b)))
            .collect();
        let kept: Vec<&(SageRecord, Option<f64>)> =
            decoded.iter().filter(|(m, _)| keep(m)).collect();
        let finals: Vec<&(SageRecord, Option<f64>)> = kept.iter().take(limit).copied().collect();
        let now_ms = parse_iso_ms(now_iso).unwrap_or(0);
        let mems: Vec<SageRecord> = finals.iter().map(|(m, _)| m.clone()).collect();
        let ranks: Vec<Option<f64>> = finals.iter().map(|(_, b)| *b).collect();
        let sc = scores(&mems, &ranks, now_ms);
        let hits: Vec<Value> = mems
            .iter()
            .zip(&ranks)
            .zip(&sc)
            .map(|((m, b), s)| hit(m, *s, *b, reason))
            .collect();

        let window_exhausted = data_rows.len() > sql_limit;
        let anchor = if kept.len() > limit {
            finals.last().copied()
        } else if window_exhausted {
            decoded.last()
        } else {
            None
        };
        let keys = sort_keys(&ranking, fts);
        let needs_bm25 = keys.contains(&Key::Bm25);
        let next_cursor = anchor.and_then(|(m, b)| {
            (!needs_bm25 || b.is_some()).then(|| {
                encode_cursor(
                    &bind,
                    &ranking,
                    channel,
                    &CursorValues {
                        bm25: if needs_bm25 { *b } else { None },
                        importance: m.importance,
                        updated_at: m.updated_at.clone(),
                        id: m.id.clone(),
                    },
                )
            })
        });

        let suggest = o.suggest.as_deref().unwrap_or("empty");
        let suggestions = if cursor.is_none()
            && suggest != "never"
            && (suggest == "always" || hits.is_empty())
            && fts
            && terms.len() >= 2
        {
            let mut clauses = vec![
                format!("m.status IN ({status_ph})"),
                "memories_fts MATCH ?".into(),
            ];
            let mut params: Vec<Sql> = statuses.iter().cloned().map(Sql::Text).collect();
            params.push(Sql::Text(terms.join(" OR ")));
            filters.push("m.", &mut clauses, &mut params);
            let slimit = if js_filters {
                (limit * JS_FILTER_OVERFETCH).min(MAX_LIMIT * 5)
            } else {
                (limit * 2).clamp(10, MAX_LIMIT)
            };
            let rows = self.rows_with_bm25(
                &format!(
                    "SELECT m.data, bm25(memories_fts) FROM memories_fts f CROSS JOIN memories m ON m.rowid = f.rowid WHERE {} ORDER BY {} LIMIT {slimit}",
                    clauses.join(" AND "),
                    order_by(&ranking, true)
                ),
                params,
                true,
            )?;
            let excluded: Vec<&str> = mems.iter().map(|m| m.id.as_str()).collect();
            let chosen: Vec<(SageRecord, Option<f64>)> = rows
                .iter()
                .filter_map(|(d, b)| decode_memory(d).map(|m| (m, *b)))
                .filter(|(m, _)| !excluded.contains(&m.id.as_str()) && keep(m))
                .take(limit)
                .collect();
            let cm: Vec<SageRecord> = chosen.iter().map(|(m, _)| m.clone()).collect();
            let cb: Vec<Option<f64>> = chosen.iter().map(|(_, b)| *b).collect();
            let cs = scores(&cm, &cb, now_ms);
            cm.iter()
                .zip(&cb)
                .zip(&cs)
                .map(|((m, b), s)| hit(m, *s, *b, "lexical"))
                .collect()
        } else {
            Vec::new()
        };

        let mut echo = Map::new();
        for k in [
            "text",
            "kinds",
            "scopes",
            "importanceAtLeast",
            "freshness",
            "paths",
            "audience",
            "anchor",
        ] {
            if let Some(v) = raw_query.get(k).filter(|v| !v.is_null()) {
                echo.insert(k.into(), v.clone());
            }
        }
        Ok(json!({
            "hits": hits,
            "suggestions": suggestions,
            "totalCandidates": total,
            "rankingApplied": ranking,
            "queryEcho": echo,
            "nextCursor": next_cursor,
            "matchChannel": channel,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyset_is_tuple_comparison() {
        let c = CursorValues {
            bm25: Some(-1.5),
            importance: 0.5,
            updated_at: "u".into(),
            id: "i".into(),
        };
        let (clause, params) = keyset(&sort_keys("hybrid", true), &c, true).unwrap();
        assert_eq!(
            clause,
            "(bm25(memories_fts) > ? OR bm25(memories_fts) = ? AND m.importance < ? OR bm25(memories_fts) = ? AND m.importance = ? AND m.updated_at < ? OR bm25(memories_fts) = ? AND m.importance = ? AND m.updated_at = ? AND m.id < ?)"
        );
        assert_eq!(params.len(), 10);
    }

    #[test]
    fn cursor_round_trip_and_strict_validation() {
        let v = CursorValues {
            bm25: None,
            importance: 0.7,
            updated_at: "2026-11-01T00:00:01.000Z".into(),
            id: "x".into(),
        };
        let t = encode_cursor("bind", "recency", "plain", &v);
        let d = decode_cursor(&t, "bind", "recency", "plain").unwrap();
        assert_eq!((d.importance, d.id.as_str()), (0.7, "x"));
        assert!(decode_cursor(&t, "other", "recency", "plain").is_err());
        assert!(decode_cursor(&t, "bind", "importance", "plain").is_err());
        assert!(decode_cursor(&t, "bind", "recency", "fts").is_err());
        assert!(decode_cursor("not-a-cursor", "bind", "recency", "plain").is_err());
        // A bm25 slot where none is expected is rejected.
        let with_b = encode_cursor(
            "bind",
            "recency",
            "plain",
            &CursorValues {
                bm25: Some(-1.0),
                ..v
            },
        );
        assert!(decode_cursor(&with_b, "bind", "recency", "plain").is_err());
    }
}
