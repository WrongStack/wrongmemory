//! Retrieval surfaces beyond lexical search (WM-500/501):
//! `retrieveForPath` (edge targets + LIKE fallback), the hashing-vector
//! hybrid rerank applied to multi-term searches, `listSagePage` with keyset
//! cursors, bounded graph traversal with hub damping, `findRelatedSage`
//! scoring, and stats.
//!
//! Visibility differs from the baseline in one deliberate way: graph
//! traversal never crosses a memory node the caller cannot see, and hidden
//! memory nodes are not returned (WM-INV-08). The baseline `graphFor`
//! exposes raw edges without a session filter.

use std::collections::{BTreeMap, HashSet};

use rusqlite::params_from_iter;
use serde::Serialize;
use serde_json::{Value, json};
use wrongmemory_contracts::{
    ErrorBody, ErrorCode, SAGE_STATUSES, SageRecord, SearchHit, SearchResult,
};

use crate::store::{SageStore, StoreResult, Viewer, decode_memory, sql_err};
use crate::text::{normalize_slashes, unicode_fold};

const GRAPH_HUB_DEGREE: usize = 64;
const SOFT_RELATIONS: &[&str] = &["same_topic", "related_to"];

fn invalid(msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::InvalidRequest, msg)
}

/// Count of memory-owned edges whose both memory ends the viewer can see.
///
/// `m.id = substr(node, 5)`, not `node = 'mem:' || m.id`: the expression
/// form cannot use the memories primary key, so it scanned every memory per
/// edge and took minutes on a real 16.5k-memory / 20.5k-edge store
/// (`stats_edge_count_looks_memories_up_by_key` checks the query plan).
fn visible_edges_sql(viewer: &Viewer) -> (String, Vec<rusqlite::types::Value>) {
    let mut args = Vec::new();
    let from_visible = session_sql("m.", viewer, &mut args);
    let to_visible = session_sql("m.", viewer, &mut args);
    (
        format!(
            "SELECT COUNT(*) FROM edges e
              WHERE e.from_node GLOB 'mem:*'
                AND NOT EXISTS (SELECT 1 FROM memories m
                      WHERE m.id = substr(e.from_node, 5)
                        AND NOT COALESCE({from_visible}, 0))
                AND NOT EXISTS (SELECT 1 FROM memories m
                      WHERE e.to_node GLOB 'mem:*' AND m.id = substr(e.to_node, 5)
                        AND NOT COALESCE({to_visible}, 0))"
        ),
        args,
    )
}

/// Whether no memory the viewer can see references the structural node's
/// subtree: either only hidden memories do, or none does. Retiring a memory
/// drops its own edges but leaves the file/dir/symbol structure its anchors
/// created, so orphan structure can name a session-private memory's paths and
/// symbols; it is hidden too (WM-603). `CROSS JOIN` pins the join order (SQLite never
/// reorders it): the walk starts from the subtree and reaches edges and
/// memories by key. Left to the planner, the memory references were found by
/// scanning every `mem:*` edge per node, about 50 ms per node on a store with
/// 20k edges (`structural_visibility_walks_from_the_subtree` checks the plan).
fn structural_hidden_sql(node: &str, viewer: &Viewer) -> (String, Vec<rusqlite::types::Value>) {
    let mut args: Vec<rusqlite::types::Value> = vec![node.to_string().into()];
    let visible = session_sql("m.", viewer, &mut args);
    (
        format!(
            "WITH RECURSIVE sub(node) AS (
               SELECT ?1
               UNION
               SELECT e.from_node FROM sub CROSS JOIN edges e ON e.to_node = sub.node
                WHERE e.from_node NOT GLOB 'mem:*'
             ),
             refs AS (
               SELECT COALESCE({visible}, 0) AS ok FROM sub
                 CROSS JOIN edges e ON e.to_node = sub.node
                 CROSS JOIN memories m ON m.id = substr(e.from_node, 5)
                WHERE e.from_node GLOB 'mem:*'
             )
             SELECT NOT EXISTS(SELECT 1 FROM refs WHERE ok = 1)"
        ),
        args,
    )
}

fn session_sql(prefix: &str, viewer: &Viewer, args: &mut Vec<rusqlite::types::Value>) -> String {
    args.push(viewer.session_id.clone().into());
    let n = args.len();
    if viewer.session_id.is_some() {
        format!("({prefix}scope != 'session' OR {prefix}owner_session_id = ?{n})")
    } else {
        format!(
            "({prefix}scope != 'session' OR ({prefix}owner_session_id IS NULL AND ?{n} IS NULL))"
        )
    }
}

fn placeholders(args: &mut Vec<rusqlite::types::Value>, values: &[String]) -> String {
    values
        .iter()
        .map(|v| {
            args.push(v.clone().into());
            format!("?{}", args.len())
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Insertion-ordered set (the baseline's JS `Set` iteration order).
#[derive(Default)]
struct OrderedSet {
    items: Vec<String>,
    seen: HashSet<String>,
}

impl OrderedSet {
    fn push(&mut self, v: String) {
        if self.seen.insert(v.clone()) {
            self.items.push(v);
        }
    }
}

/// `escapeLikeLiteral`: escape `\`, `%` and `_` for a LIKE prefix pattern.
fn escape_like_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(crate) fn escape_glob(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '*' | '?' | '[' | ']' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            _ => out.push(c),
        }
    }
    out
}

fn escape_like(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ─── Hybrid rerank (retrieval/hybrid-rerank.ts + embeddings/hashing.ts) ────

const DIMENSIONS: usize = 256;

fn fnv1a_utf16(token: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for unit in token.encode_utf16() {
        hash ^= unit as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// `HashingEmbeddingProvider(256)`: NFKC/lowercase tokens split on
/// `[^\p{L}\p{N}_]+`, FNV-1a over UTF-16 units, log1p counts, L2 norm (f32).
pub fn hashing_embed(text: &str) -> [f32; DIMENSIONS] {
    let mut v = [0f32; DIMENSIONS];
    if text.is_empty() {
        return v;
    }
    for token in
        unicode_fold(text).split(|c: char| !(c.is_alphabetic() || c.is_numeric() || c == '_'))
    {
        if !token.is_empty() {
            v[(fnv1a_utf16(token) % DIMENSIONS as u32) as usize] += 1.0;
        }
    }
    for x in v.iter_mut() {
        *x = x.ln_1p();
    }
    let norm = v
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x = ((*x as f64) / norm) as f32;
        }
    }
    v
}

fn cosine(a: &[f32; DIMENSIONS], b: &[f32; DIMENSIONS]) -> f64 {
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for i in 0..DIMENSIONS {
        dot += a[i] as f64 * b[i] as f64;
        na += a[i] as f64 * a[i] as f64;
        nb += b[i] as f64 * b[i] as f64;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// `hybridRerankMemories(query, candidates, 0.25)`; stable sort.
pub fn hybrid_rerank(query: &str, hits: &mut Vec<SearchHit>) {
    let q = query.trim();
    if hits.len() <= 1 || q.chars().count() < 3 {
        return;
    }
    let weight = 0.25;
    let qv = hashing_embed(q);
    let n = hits.len();
    let mut scored: Vec<(f64, SearchHit)> = hits
        .drain(..)
        .enumerate()
        .map(|(i, h)| {
            let text: String = h
                .memory
                .text
                .encode_utf16()
                .take(2000)
                .collect::<Vec<u16>>()
                .pipe_utf16();
            let c = cosine(&qv, &hashing_embed(&text)).max(0.0);
            let position = 1.0 - i as f64 / (n.max(1)) as f64;
            ((1.0 - weight) * position + weight * c, h)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    for (s, mut h) in scored {
        h.score = s;
        h.reasons.push("hybrid-rerank".into());
        hits.push(h);
    }
}

trait PipeUtf16 {
    fn pipe_utf16(self) -> String;
}

impl PipeUtf16 for Vec<u16> {
    fn pipe_utf16(self) -> String {
        String::from_utf16_lossy(&self)
    }
}

// ─── Page listing ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageResult {
    pub memories: Vec<SageRecord>,
    pub next_cursor: Option<String>,
    pub total: i64,
    pub status_counts: BTreeMap<String, i64>,
    pub corrupt_skipped: usize,
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..=chunk.len() {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut out = Vec::new();
    for c in s.trim_end_matches('=').bytes() {
        let v = B64.iter().position(|x| *x == c)? as u32;
        bits = (bits << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
            bits &= (1 << nbits) - 1;
        }
    }
    Some(out)
}

/// `listSagePage` cursor: base64url of `{"u": updatedAt, "i": id}`.
pub fn encode_page_cursor(updated_at: &str, id: &str) -> String {
    base64url_encode(json!({"u": updated_at, "i": id}).to_string().as_bytes())
}

fn decode_page_cursor(c: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_slice(&base64url_decode(c)?).ok()?;
    Some((
        v.get("u")?.as_str()?.to_string(),
        v.get("i")?.as_str()?.to_string(),
    ))
}

#[derive(Debug, Clone, Default)]
pub struct PathOptions {
    pub limit: Option<usize>,
    pub include_ancestors: Option<bool>,
    pub statuses: Option<Vec<String>>,
    pub include_audience_scoped: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub relation: String,
    pub weight: f64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphView {
    pub edges: Vec<GraphEdge>,
    pub memories: Vec<SageRecord>,
    pub truncated: bool,
}

impl SageStore {
    /// `retrieveForPath`: memories whose `mem:` edges reach the path's file,
    /// symbols, or ancestor file/dir nodes; JSON-path LIKE fallback.
    pub fn retrieve_for_paths(
        &self,
        paths: &[String],
        opts: &PathOptions,
        viewer: &Viewer,
    ) -> StoreResult<SearchResult> {
        let limit = opts
            .limit
            .unwrap_or(50)
            .clamp(1, wrongmemory_contracts::MAX_PAGE_SIZE);
        let ancestors = opts.include_ancestors.unwrap_or(true);
        let statuses = opts
            .statuses
            .clone()
            .unwrap_or_else(|| vec!["active".into(), "stale".into()]);
        if statuses
            .iter()
            .any(|s| !SAGE_STATUSES.contains(&s.as_str()))
        {
            return Err(invalid("unknown status"));
        }
        let rel: Vec<String> = paths
            .iter()
            .map(|p| self.relative_path(p))
            .filter(|p| !p.is_empty() && !p.split('/').any(|seg| seg == ".."))
            .collect();
        if rel.is_empty() || statuses.is_empty() {
            return Ok(SearchResult {
                hits: vec![],
                truncated: false,
                corrupt_skipped: 0,
                channel: "path".into(),
            });
        }
        let mut targets: Vec<String> = Vec::new();
        let mut symbol_prefixes: Vec<String> = Vec::new();
        let mut add = |t: String| {
            if !targets.contains(&t) {
                targets.push(t);
            }
        };
        for p in &rel {
            add(format!("file:{p}"));
            symbol_prefixes.push(format!("symbol:{p}"));
            if ancestors {
                let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
                for i in (1..=parts.len()).rev() {
                    let a = parts[..i].join("/");
                    add(format!("file:{a}"));
                    add(format!("dir:{a}"));
                }
            }
        }
        let audience_ok = opts.include_audience_scoped != Some(false);
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        let target_ph = placeholders(&mut args, &targets);
        // Same rows as the baseline `m.id IN (edges: to_node IN targets OR
        // to_node GLOB 'symbol:<p>#*')`, planned so the work is bounded by the
        // matching edges (WM-604; the IN form walked every memory, 45 ms p50
        // at 25k):
        // * the GLOB is exactly the BINARY prefix range ['symbol:p#',
        //   'symbol:p$'), one UNION ALL arm per path;
        // * every arm is pinned to `idx_edge_to` (the planner otherwise picks
        //   the `from_node` range implied by `GLOB 'mem:*'`);
        // * the hit ids are materialized and joined to memories by primary
        //   key; `DISTINCT m.data` absorbs duplicate hits as before.
        let symbol_arms: String = symbol_prefixes
            .iter()
            .map(|p| {
                args.push(format!("{p}#").into());
                let lo = args.len();
                args.push(format!("{p}$").into());
                format!(
                    " UNION ALL SELECT SUBSTR(e.from_node, 5) FROM edges e INDEXED BY idx_edge_to
                       WHERE e.to_node >= ?{lo} AND e.to_node < ?{} AND e.from_node GLOB 'mem:*'",
                    lo + 1
                )
            })
            .collect();
        let status_ph = placeholders(&mut args, &statuses);
        let session = session_sql("m.", viewer, &mut args);
        let sql = format!(
            "WITH hit(id) AS MATERIALIZED (
                SELECT SUBSTR(e.from_node, 5) FROM edges e INDEXED BY idx_edge_to
                 WHERE e.to_node IN ({target_ph}) AND e.from_node GLOB 'mem:*'{})
             SELECT DISTINCT m.data FROM hit CROSS JOIN memories m ON m.id = hit.id
              WHERE m.status IN ({status_ph}) AND {session}{}
              ORDER BY m.importance DESC, m.updated_at DESC LIMIT {}",
            symbol_arms,
            if audience_ok {
                ""
            } else {
                " AND m.audience IS NULL"
            },
            limit + 1
        );
        let mut rows = self.query_data(&sql, args)?;
        let mut channel = "path";
        if rows.is_empty() {
            channel = "path-fallback";
            let mut args: Vec<rusqlite::types::Value> = Vec::new();
            let status_ph = placeholders(&mut args, &statuses);
            let mut conds = Vec::new();
            for p in &rel {
                let mut list = vec![p.clone()];
                if ancestors {
                    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
                    for i in (1..=parts.len()).rev() {
                        list.push(parts[..i].join("/"));
                    }
                }
                for q in list {
                    args.push(
                        format!("%\"path\":{}%", escape_like(&Value::String(q).to_string())).into(),
                    );
                    conds.push(format!("LOWER(data) LIKE ?{} ESCAPE '\\'", args.len()));
                }
            }
            let session = session_sql("", viewer, &mut args);
            let sql =
                format!(
                "SELECT data FROM memories WHERE status IN ({status_ph}) AND ({}) AND {session}{}
                  ORDER BY importance DESC, updated_at DESC LIMIT {}",
                conds.join(" OR "),
                if audience_ok { "" } else { " AND audience IS NULL" },
                limit + 1
            );
            rows = self.query_data(&sql, args)?;
        }
        let mut hits = Vec::new();
        let mut corrupt = 0;
        for (rank, d) in rows.iter().enumerate() {
            match decode_memory(d) {
                Some(m) if audience_ok || m.audience.is_none() => hits.push(SearchHit {
                    memory: m,
                    channel: channel.into(),
                    score: 1.0 / (rank as f64 + 1.0),
                    reasons: vec![],
                }),
                Some(_) => {}
                None => corrupt += 1,
            }
        }
        let truncated = hits.len() > limit;
        hits.truncate(limit);
        Ok(SearchResult {
            hits,
            truncated,
            corrupt_skipped: corrupt,
            channel: channel.into(),
        })
    }

    fn query_data(&self, sql: &str, args: Vec<rusqlite::types::Value>) -> StoreResult<Vec<String>> {
        let mut stmt = self.conn.prepare(sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(params_from_iter(args), |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)
    }

    /// `listSagePage` (management listing). Statuses default to all except
    /// `deleted`; order `updatedAt DESC, id DESC`; limit clamped to [1, 500].
    pub fn list_page(
        &self,
        statuses: Option<&[String]>,
        kind: Option<&str>,
        query: Option<&str>,
        limit: Option<usize>,
        cursor: Option<&str>,
        viewer: &Viewer,
    ) -> StoreResult<PageResult> {
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        let session = session_sql("", viewer, &mut args);
        let mut status_counts = BTreeMap::new();
        {
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT status, COUNT(*) FROM memories WHERE {session} GROUP BY status"
                ))
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params_from_iter(args.clone()), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(sql_err)?;
            for r in rows {
                let (k, v) = r.map_err(sql_err)?;
                status_counts.insert(k, v);
            }
        }
        let default: Vec<String> = SAGE_STATUSES
            .iter()
            .filter(|s| **s != "deleted")
            .map(|s| s.to_string())
            .collect();
        let statuses: Vec<String> = statuses.map(<[String]>::to_vec).unwrap_or(default);
        if statuses
            .iter()
            .any(|s| !SAGE_STATUSES.contains(&s.as_str()))
        {
            return Err(invalid("unknown status"));
        }
        if statuses.is_empty() {
            return Ok(PageResult {
                memories: vec![],
                next_cursor: None,
                total: 0,
                status_counts,
                corrupt_skipped: 0,
            });
        }
        let limit = limit.unwrap_or(50).clamp(1, 500);
        let mut where_ = vec![session];
        where_.push(format!(
            "status IN ({})",
            placeholders(&mut args, &statuses)
        ));
        if let Some(k) = kind.filter(|k| *k != "all") {
            args.push(k.to_string().into());
            where_.push(format!("kind = ?{}", args.len()));
        }
        let q = query
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(unicode_fold);
        if let Some(q) = &q {
            args.push(format!("%{}%", escape_like(q)).into());
            // Folded on the fly so read-only handles need no projection refresh.
            where_.push(format!(
                "COALESCE(unicode_text, LOWER(CASE WHEN json_valid(data) THEN json_extract(data, '$.text') END)) LIKE ?{} ESCAPE '\\'",
                args.len()
            ));
        }
        let where_sql = where_.join(" AND ");
        let total: i64 = self
            .conn
            .query_row(
                &format!("SELECT COUNT(*) FROM memories WHERE {where_sql}"),
                params_from_iter(args.clone()),
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        let mut page_sql = format!("SELECT data, updated_at, id FROM memories WHERE {where_sql}");
        if let Some((u, i)) = cursor.and_then(decode_page_cursor) {
            args.push(u.into());
            let un = args.len();
            args.push(i.into());
            page_sql.push_str(&format!(
                " AND (updated_at < ?{un} OR (updated_at = ?{un} AND id < ?{}))",
                args.len()
            ));
        }
        page_sql.push_str(&format!(
            " ORDER BY updated_at DESC, id DESC LIMIT {}",
            limit + 1
        ));
        let mut stmt = self.conn.prepare(&page_sql).map_err(sql_err)?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map(params_from_iter(args), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(sql_err)?;
        let has_more = rows.len() > limit;
        let page = &rows[..rows.len().min(limit)];
        let next_cursor = if has_more {
            page.last().map(|(_, u, i)| encode_page_cursor(u, i))
        } else {
            None
        };
        let mut memories = Vec::new();
        let mut corrupt = 0;
        for (d, _, _) in page {
            match decode_memory(d) {
                Some(m) => memories.push(m),
                None => corrupt += 1,
            }
        }
        Ok(PageResult {
            memories,
            next_cursor,
            total,
            status_counts,
            corrupt_skipped: corrupt,
        })
    }

    /// Ids of memory nodes the viewer may see (any status except where noted).
    fn visible_memory_ids(&self, ids: &[String], viewer: &Viewer) -> StoreResult<HashSet<String>> {
        let mut out = HashSet::new();
        for chunk in ids.chunks(400) {
            let mut args: Vec<rusqlite::types::Value> = Vec::new();
            let ph = placeholders(&mut args, chunk);
            let session = session_sql("", viewer, &mut args);
            let mut stmt = self
                .conn
                .prepare(&format!("SELECT id FROM memories WHERE id IN ({ph}) AND status != 'deleted' AND {session}"))
                .map_err(sql_err)?;
            for r in stmt
                .query_map(params_from_iter(args), |r| r.get::<_, String>(0))
                .map_err(sql_err)?
            {
                out.insert(r.map_err(sql_err)?);
            }
        }
        Ok(out)
    }

    fn classify(
        &self,
        nodes: &[String],
        viewer: &Viewer,
        hidden: &mut HashSet<String>,
        allowed: &mut HashSet<String>,
    ) -> StoreResult<()> {
        // Structural nodes (file/dir/symbol/...) are shared between memories
        // but are derived from their anchors. A node whose whole subtree is
        // referenced only by memories this viewer cannot see would disclose a
        // hidden anchor (e.g. a private symbol name), so it is hidden too
        // (WM-603). So is orphan structure left behind by retired memories,
        // which can name a private memory's paths. Anything a visible memory
        // supports stays visible.
        for n in nodes {
            if n.starts_with("mem:") || hidden.contains(n) || allowed.contains(n) {
                continue;
            }
            if self.structural_node_hidden(n, viewer)? {
                hidden.insert(n.clone());
            } else {
                allowed.insert(n.clone());
            }
        }
        let mut ids: Vec<String> = nodes
            .iter()
            .filter_map(|n| n.strip_prefix("mem:"))
            .filter(|id| !hidden.contains(*id) && !allowed.contains(*id))
            .map(str::to_string)
            .collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            return Ok(());
        }
        let ok = self.visible_memory_ids(&ids, viewer)?;
        for id in ids {
            if ok.contains(&id) {
                allowed.insert(id);
            } else {
                hidden.insert(id);
            }
        }
        Ok(())
    }

    fn structural_node_hidden(&self, node: &str, viewer: &Viewer) -> StoreResult<bool> {
        let (sql, args) = structural_hidden_sql(node, viewer);
        self.conn
            .query_row(&sql, params_from_iter(args), |r| r.get::<_, bool>(0))
            .map_err(sql_err)
    }

    /// `traverseSqliteGraph` with hub damping, made visibility-safe: hidden
    /// or deleted memory nodes are neither returned nor traversed through.
    pub fn traverse(
        &self,
        starts: &[String],
        max_depth: usize,
        limit: usize,
        viewer: &Viewer,
    ) -> StoreResult<(Vec<GraphEdge>, bool)> {
        let max_depth = max_depth.clamp(1, 6);
        let limit = limit.clamp(1, 1000);
        let start_set: HashSet<String> = starts.iter().cloned().collect();
        let mut hidden: HashSet<String> = HashSet::new();
        let mut allowed: HashSet<String> = HashSet::new();
        let is_hidden = |hidden: &HashSet<String>, n: &str| {
            n.strip_prefix("mem:")
                .map_or_else(|| hidden.contains(n), |id| hidden.contains(id))
        };
        self.classify(starts, viewer, &mut hidden, &mut allowed)?;
        let mut visited_nodes: HashSet<String> = start_set.clone();
        let mut visited_edges: HashSet<String> = HashSet::new();
        let mut result: Vec<GraphEdge> = Vec::new();
        let mut frontier: Vec<String> = starts
            .iter()
            .filter(|s| !is_hidden(&hidden, s))
            .cloned()
            .collect();
        let mut depth = 0;
        let mut truncated = false;
        while !frontier.is_empty() && depth < max_depth {
            if result.len() >= limit {
                truncated = true;
                break;
            }
            let mut next_frontier = Vec::new();
            for chunk in frontier.chunks(200) {
                let mut args: Vec<rusqlite::types::Value> = Vec::new();
                let ph = placeholders(&mut args, chunk);
                let sql = format!(
                    "SELECT from_node, to_node, relation, weight, created_at FROM edges WHERE from_node IN ({ph}) OR to_node IN ({ph})"
                );
                let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
                let rows: Vec<GraphEdge> = stmt
                    .query_map(params_from_iter(args), |r| {
                        Ok(GraphEdge {
                            from: r.get(0)?,
                            to: r.get(1)?,
                            relation: r.get(2)?,
                            weight: r.get(3)?,
                            created_at: r.get(4)?,
                        })
                    })
                    .map_err(sql_err)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(sql_err)?;
                let frontier_set: HashSet<&String> = chunk.iter().collect();
                // Hub damping (GRAPH_HUB_DEGREE soft edges per owner).
                let rows = if rows.len() <= GRAPH_HUB_DEGREE {
                    rows
                } else {
                    let mut by_owner: BTreeMap<String, Vec<GraphEdge>> = BTreeMap::new();
                    for r in rows {
                        let owner = if frontier_set.contains(&r.from) {
                            r.from.clone()
                        } else {
                            r.to.clone()
                        };
                        by_owner.entry(owner).or_default().push(r);
                    }
                    let mut kept = Vec::new();
                    for (owner, mut bucket) in by_owner {
                        let soft = bucket
                            .iter()
                            .filter(|r| SOFT_RELATIONS.contains(&r.relation.as_str()))
                            .count();
                        if soft <= GRAPH_HUB_DEGREE {
                            kept.extend(bucket);
                        } else if start_set.contains(&owner) {
                            bucket.sort_by(|a, b| {
                                b.weight
                                    .partial_cmp(&a.weight)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                            kept.extend(bucket.into_iter().take(GRAPH_HUB_DEGREE));
                        }
                    }
                    kept
                };
                let nodes: Vec<String> = rows
                    .iter()
                    .flat_map(|r| [r.from.clone(), r.to.clone()])
                    .collect();
                self.classify(&nodes, viewer, &mut hidden, &mut allowed)?;
                for r in rows {
                    if result.len() >= limit {
                        truncated = true;
                        break;
                    }
                    if is_hidden(&hidden, &r.from) || is_hidden(&hidden, &r.to) {
                        continue;
                    }
                    let key = format!("{}\0{}\0{}", r.from, r.to, r.relation);
                    if !visited_edges.insert(key) {
                        continue;
                    }
                    let next = if frontier_set.contains(&r.from) && !frontier_set.contains(&r.to) {
                        Some(r.to.clone())
                    } else if frontier_set.contains(&r.to) && !frontier_set.contains(&r.from) {
                        Some(r.from.clone())
                    } else {
                        None
                    };
                    if let Some(n) = next
                        && visited_nodes.insert(n.clone())
                    {
                        next_frontier.push(n);
                    }
                    result.push(r);
                }
            }
            frontier = next_frontier;
            depth += 1;
        }
        Ok((result, truncated))
    }

    /// `graphFor`: starts from a node id, memory id, path, or search hits.
    pub fn graph_for(
        &self,
        query: &str,
        max_depth: usize,
        limit: usize,
        viewer: &Viewer,
    ) -> StoreResult<GraphView> {
        let q = query.trim();
        let mut starts: Vec<String> = Vec::new();
        let mut add = |s: String| {
            if !starts.contains(&s) {
                starts.push(s);
            }
        };
        let prefixes = [
            "mem:", "file:", "dir:", "symbol:", "command:", "session:", "tool:", "agent:",
        ];
        if prefixes.iter().any(|p| q.starts_with(p)) {
            add(normalize_slashes(q));
        } else if !q.is_empty() && q.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            add(format!("mem:{q}"));
        }
        if !q.is_empty() && !prefixes.iter().any(|p| q.starts_with(p)) {
            let rel = self.relative_path(q);
            if !rel.is_empty() && !rel.split('/').any(|s| s == "..") {
                add(format!("file:{rel}"));
                add(format!("dir:{rel}"));
                let symbols = self.query_data(
                    "SELECT DISTINCT to_node FROM edges WHERE to_node GLOB ?1",
                    vec![format!("symbol:{}#*", escape_glob(&rel)).into()],
                )?;
                for s in symbols {
                    add(s);
                }
            }
        }
        for h in self.search(q, 20, None, None, viewer)?.hits {
            add(format!("mem:{}", h.memory.id));
        }
        // Baseline: the anchors of every memory start (direct id or search
        // hit) are added after all other starts. Reading through `get`
        // keeps hidden memories from contributing their anchors.
        let mem_ids: Vec<String> = starts
            .iter()
            .filter_map(|s| s.strip_prefix("mem:"))
            .map(str::to_string)
            .collect();
        for id in mem_ids {
            if let Some(m) = self.get(&id, viewer)? {
                for a in &m.anchors {
                    if let Some(n) = crate::store::anchor_node(a)
                        && !starts.contains(&n)
                    {
                        starts.push(n);
                    }
                }
            }
        }
        let (edges, truncated) = self.traverse(&starts, max_depth, limit, viewer)?;
        let mut ids: Vec<String> = edges
            .iter()
            .flat_map(|e| [&e.from, &e.to])
            .filter_map(|n| n.strip_prefix("mem:"))
            .map(str::to_string)
            .collect();
        ids.sort();
        ids.dedup();
        let mut memories = Vec::new();
        for id in ids.iter().take(100) {
            if let Some(m) = self.get(id, viewer)? {
                memories.push(m);
            }
        }
        Ok(GraphView {
            edges,
            memories,
            truncated,
        })
    }

    /// `findRelatedSqliteSage` + `collectRelatedSqliteCandidateIds`
    /// (baseline `sqlite-store-find-related.ts` /
    /// `sqlite-store-related-candidates.ts`), visibility-safe: seeds and
    /// candidates are read through the session rule, and the traversal never
    /// passes through hidden memories.
    ///
    /// Candidates: graph neighbours (BFS budget `max(100, limit*20)`),
    /// memories sharing an `about_*` target, anchor node or anchor-path
    /// ancestor with a seed, co-tagged memories (importance-ordered, capped
    /// at the budget) and memories in the same command family. Ranked by
    /// `scoreMemoryRelationship`, then importance, then confidence.
    pub fn find_related(
        &self,
        ids: &[String],
        limit: usize,
        max_depth: usize,
        viewer: &Viewer,
    ) -> StoreResult<Vec<SearchHit>> {
        let statuses = ["active".to_string()];
        let limit = limit.clamp(1, wrongmemory_contracts::MAX_PAGE_SIZE);
        let mut seeds: Vec<SageRecord> = Vec::new();
        for id in ids {
            if let Some(m) = self.get(id, viewer)?
                && !seeds.iter().any(|s| s.id == m.id)
            {
                seeds.push(m);
            }
        }
        if seeds.is_empty() {
            return Ok(vec![]);
        }
        let budget = (limit * 20).max(100);
        let result_cap = limit.min(budget).max(1);
        let starts: Vec<String> = seeds.iter().map(|s| format!("mem:{}", s.id)).collect();
        let (edges, _) = self.traverse(&starts, max_depth.max(1), budget, viewer)?;

        // Insertion-ordered candidate id set, as the baseline's `Set`.
        let mut cand = OrderedSet::default();
        let mut graph: HashSet<String> = HashSet::new();
        for e in &edges {
            for n in [&e.from, &e.to] {
                if let Some(id) = n.strip_prefix("mem:") {
                    graph.insert(id.to_string());
                    cand.push(id.to_string());
                }
            }
        }

        // Anchor-overlap targets: the seeds' `about_*` edge targets, their
        // anchor nodes, and every ancestor of an anchor path.
        let mut targets = OrderedSet::default();
        {
            let mut args = Vec::new();
            let ph = placeholders(&mut args, &starts);
            for n in self.query_data(
                &format!(
                    "SELECT DISTINCT to_node FROM edges
                      WHERE from_node IN ({ph}) AND relation GLOB 'about_*'"
                ),
                args,
            )? {
                targets.push(n);
            }
        }
        for s in &seeds {
            for a in &s.anchors {
                if let Some(n) = crate::store::anchor_node(a) {
                    targets.push(n);
                }
                if let Some(p) = a.path.as_deref() {
                    let normalized = normalize_slashes(p);
                    let parts: Vec<&str> =
                        normalized.split('/').filter(|x| !x.is_empty()).collect();
                    for i in 1..parts.len() {
                        let anc = parts[..i].join("/");
                        targets.push(format!("file:{anc}"));
                        targets.push(format!("dir:{anc}"));
                    }
                }
            }
        }
        for chunk in targets.items.chunks(400) {
            let mut args = Vec::new();
            let ph = placeholders(&mut args, chunk);
            for n in self.query_data(
                &format!(
                    "SELECT DISTINCT from_node FROM edges INDEXED BY idx_edge_to
                      WHERE to_node IN ({ph}) AND from_node GLOB 'mem:*'
                        AND relation GLOB 'about_*'"
                ),
                args,
            )? {
                cand.push(n["mem:".len()..].to_string());
            }
        }

        // Co-tagged memories, importance-ordered and capped at the budget.
        let mut tags = OrderedSet::default();
        for s in &seeds {
            for t in &s.tags {
                tags.push(t.clone());
            }
        }
        for t in &tags.items {
            let mut args: Vec<rusqlite::types::Value> = Vec::new();
            let status_ph = placeholders(&mut args, &statuses);
            args.push(t.clone().into());
            let tag_n = args.len();
            let session = session_sql("m.", viewer, &mut args);
            for id in self.query_data(
                &format!(
                    "SELECT m.id FROM memories m
                      WHERE m.status IN ({status_ph}) AND json_valid(m.tags)
                        AND EXISTS (SELECT 1 FROM json_each(m.tags) je WHERE je.value = ?{tag_n})
                        AND {session}
                      ORDER BY m.importance DESC, m.updated_at DESC LIMIT {budget}"
                ),
                args,
            )? {
                cand.push(id);
            }
        }

        // Same command family: LIKE prefilter, exact family comparison.
        let family_of =
            |c: &str| crate::similarity::command_family(&crate::similarity::normalize_command(c));
        let mut families = OrderedSet::default();
        for s in &seeds {
            for a in &s.anchors {
                if let Some(c) = a.command.as_deref() {
                    families.push(family_of(c));
                }
            }
        }
        for chunk in families.items.chunks(64) {
            let mut args: Vec<rusqlite::types::Value> = Vec::new();
            let clauses: Vec<String> = chunk
                .iter()
                .map(|f| {
                    args.push(format!("command:{}%", escape_like_literal(f)).into());
                    format!("to_node LIKE ?{} ESCAPE '\\'", args.len())
                })
                .collect();
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT DISTINCT from_node, to_node FROM edges
                      WHERE relation = 'about_command' AND from_node GLOB 'mem:*'
                        AND ({} OR to_node GLOB '*[^ -~]*')",
                    clauses.join(" OR ")
                ))
                .map_err(sql_err)?;
            let rows: Vec<(String, String)> = stmt
                .query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<_>>()
                .map_err(sql_err)?;
            for (from, to) in rows {
                let cmd = to.strip_prefix("command:").unwrap_or("");
                if !cmd.is_empty() && families.items.contains(&family_of(cmd)) {
                    cand.push(from["mem:".len()..].to_string());
                }
            }
        }
        let seed_ids: HashSet<&str> = seeds.iter().map(|s| s.id.as_str()).collect();
        cand.items.retain(|id| !seed_ids.contains(id.as_str()));
        if cand.items.is_empty() {
            return Ok(vec![]);
        }

        // Final fetch with statuses and the session rule, chunked like the
        // baseline (rows of one chunk come back in primary-key order).
        let mut candidates: Vec<SageRecord> = Vec::new();
        for chunk in cand.items.chunks(400) {
            let mut args: Vec<rusqlite::types::Value> = Vec::new();
            let id_ph = placeholders(&mut args, chunk);
            let status_ph = placeholders(&mut args, &statuses);
            let session = session_sql("", viewer, &mut args);
            for d in self.query_data(
                &format!(
                    "SELECT data FROM memories
                      WHERE id IN ({id_ph}) AND status IN ({status_ph}) AND {session}"
                ),
                args,
            )? {
                if let Some(m) = decode_memory(&d) {
                    candidates.push(m);
                }
            }
        }
        let seed_refs: Vec<&SageRecord> = seeds.iter().collect();
        let mut hits: Vec<SearchHit> = candidates
            .into_iter()
            .filter(|m| !seed_ids.contains(m.id.as_str()))
            .filter(|m| m.context_policy.as_deref() != Some("never"))
            .filter_map(|m| {
                let score = crate::similarity::score_relationship(&m, &seed_refs, &graph);
                (score > 0.0).then_some(SearchHit {
                    memory: m,
                    channel: "related".into(),
                    score,
                    reasons: vec![],
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(b.memory.importance.total_cmp(&a.memory.importance))
                .then(b.memory.confidence.total_cmp(&a.memory.confidence))
        });
        hits.truncate(result_cap);
        Ok(hits)
    }

    /// `getStats`-style counts visible to the caller.
    pub fn stats(&self, viewer: &Viewer) -> StoreResult<Value> {
        let page = self.list_page(Some(&[]), None, None, Some(1), None, viewer)?;
        let mut counts = page.status_counts;
        for s in SAGE_STATUSES {
            counts.entry((*s).to_string()).or_insert(0);
        }
        let mut args = Vec::new();
        let session = session_sql("", viewer, &mut args);
        let mut by_kind = BTreeMap::new();
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT kind, COUNT(*) FROM memories WHERE status != 'deleted' AND {session} GROUP BY kind"))
            .map_err(sql_err)?;
        for r in stmt
            .query_map(params_from_iter(args), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(sql_err)?
        {
            let (k, v) = r.map_err(sql_err)?;
            by_kind.insert(k, v);
        }
        // Counts are an existence oracle: session-private candidates and
        // edges touching a hidden memory are excluded like the rows (WM-603).
        // Structural anchor edges (symbol->file->dir) carry no owner, so only
        // memory-owned edges are counted.
        let pending: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM candidates WHERE status = 'pending'
                   AND (NOT json_valid(data)
                        OR json_extract(data, '$.scope') IS NOT 'session'
                        OR (json_extract(data, '$.ownerSessionId') IS NOT NULL
                            AND json_extract(data, '$.ownerSessionId') = ?1))",
                rusqlite::params![viewer.session_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        let (sql, args) = visible_edges_sql(viewer);
        let edges: i64 = self
            .conn
            .query_row(&sql, params_from_iter(args), |r| r.get(0))
            .map_err(sql_err)?;
        Ok(
            json!({"byStatus": counts, "byKind": by_kind, "pendingCandidates": pending, "edges": edges}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stats edge count must look memories up by primary key, never scan
    /// them per edge (it took minutes on a real store before).
    #[test]
    fn stats_edge_count_looks_memories_up_by_key() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("sage.db");
        SageStore::create_managed(&db, "2026-11-01T00:00:00.000Z").unwrap();
        let (store, _) = SageStore::open(&db, true, None, "2026-11-01T00:00:00.000Z").unwrap();
        for viewer in [
            Viewer::default(),
            Viewer {
                session_id: Some("s1".into()),
            },
        ] {
            let (sql, args) = visible_edges_sql(&viewer);
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let plan: Vec<String> = stmt
                .query_map(params_from_iter(args), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(
                !plan.iter().any(|d| d.starts_with("SCAN m")),
                "memories scanned per edge: {plan:?}"
            );
            assert!(
                plan.iter().filter(|d| d.starts_with("SEARCH m")).count() >= 2,
                "memories not looked up by key: {plan:?}"
            );
        }
    }

    #[test]
    fn structural_visibility_walks_from_the_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("sage.db");
        SageStore::create_managed(&db, "2026-11-01T00:00:00.000Z").unwrap();
        let (store, _) = SageStore::open(&db, true, None, "2026-11-01T00:00:00.000Z").unwrap();
        for viewer in [
            Viewer::default(),
            Viewer {
                session_id: Some("s1".into()),
            },
        ] {
            let (sql, args) = structural_hidden_sql("dir:src", &viewer);
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let plan: Vec<String> = stmt
                .query_map(params_from_iter(args), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(
                !plan
                    .iter()
                    .any(|d| d.starts_with("SCAN e") || d.contains("from_node>?")),
                "edges scanned per node: {plan:?}"
            );
            assert!(
                !plan.iter().any(|d| d.starts_with("SCAN m")),
                "memories scanned: {plan:?}"
            );
        }
    }

    #[test]
    fn base64url_round_trip_and_cursor() {
        for s in ["", "a", "ab", "abc", "çağrı 🚀"] {
            assert_eq!(
                base64url_decode(&base64url_encode(s.as_bytes())).unwrap(),
                s.as_bytes()
            );
        }
        // Buffer.from('{"u":"x","i":"y"}').toString('base64url')
        assert_eq!(encode_page_cursor("x", "y"), "eyJ1IjoieCIsImkiOiJ5In0");
        assert_eq!(
            decode_page_cursor("eyJ1IjoieCIsImkiOiJ5In0"),
            Some(("x".into(), "y".into()))
        );
        assert!(decode_page_cursor("not-a-cursor").is_none());
    }

    #[test]
    fn hashing_embedding_is_normalized() {
        let v = hashing_embed("Use bun run for scripts");
        let n: f64 = v.iter().map(|x| (*x as f64).powi(2)).sum();
        assert!((n - 1.0).abs() < 1e-5);
        assert_eq!(hashing_embed(""), [0f32; DIMENSIONS]);
    }
}

/// Result of [`SageStore::retrieve_for_audience`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudienceResult {
    pub memories: Vec<SageRecord>,
    /// Set when the scan stopped before the corpus ran out and the page may
    /// be incomplete (the baseline `onTruncated` signal).
    pub truncated: Option<AudienceTruncation>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudienceTruncation {
    pub sql_rows_examined: usize,
    pub returned: usize,
}

const AUDIENCE_OVERFETCH_FACTOR: usize = 5;
const AUDIENCE_MAX_SCAN: usize = 10_000;
/// Characters kept per context field in the truncation audit record.
const AUDIENCE_AUDIT_FIELD_MAX: usize = 128;

impl SageStore {
    /// `retrieveSqliteSageForAudience` + `retrieveSageAudienceWithAudit`
    /// (baseline `sqlite-store-audience.ts` / `sqlite-store-recall.ts`):
    /// importance-ordered windows of audience-carrying rows (×5 then ×3,
    /// at most 10,000 rows scanned), matched case-insensitively on role,
    /// task type and mode. A truncated scan writes `memory.audience_truncated`
    /// on writable stores, as the baseline audits it.
    ///
    /// `context` is echoed into the audit record as given; only its string
    /// `role`, `taskType` and `mode` fields drive matching.
    pub fn retrieve_for_audience(
        &self,
        context: &Value,
        limit: Option<i64>,
        viewer: &Viewer,
        now_iso: &str,
    ) -> StoreResult<AudienceResult> {
        // boundedLimit(limit, 20, AUDIENCE_MAX_SCAN)
        let limit = match limit {
            None => 20,
            Some(l) => l.clamp(0, AUDIENCE_MAX_SCAN as i64) as usize,
        };
        let field = |k: &str| {
            context
                .get(k)
                .and_then(Value::as_str)
                .map(str::to_lowercase)
                .unwrap_or_default()
        };
        let (role, task_type, mode) = (field("role"), field("taskType"), field("mode"));
        let matches = |m: &SageRecord| -> bool {
            let Some(a) = &m.audience else { return false };
            let ok = |list: &Option<Vec<String>>, want: &str| match list {
                Some(l) if !l.is_empty() => l.iter().any(|x| x.to_lowercase() == want),
                _ => true,
            };
            ok(&a.roles, &role) && ok(&a.task_types, &task_type) && ok(&a.modes, &mode)
        };

        let mut offset = 0usize;
        let mut window = limit * AUDIENCE_OVERFETCH_FACTOR;
        let mut matched: Vec<SageRecord> = Vec::new();
        let mut exhausted = false;
        let mut scanned = 0usize;
        while matched.len() < limit && offset < AUDIENCE_MAX_SCAN {
            window = window.min(AUDIENCE_MAX_SCAN - offset);
            if window == 0 {
                break;
            }
            let mut args: Vec<rusqlite::types::Value> = Vec::new();
            let session = session_sql("", viewer, &mut args);
            args.push((window as i64).into());
            args.push((offset as i64).into());
            let n = args.len();
            let rows = self.query_data(
                &format!(
                    "SELECT data FROM memories
                      WHERE status IN ('active','stale') AND audience IS NOT NULL AND {session}
                      ORDER BY importance DESC
                      LIMIT ?{} OFFSET ?{n}",
                    n - 1
                ),
                args,
            )?;
            scanned += rows.len();
            if rows.len() < window {
                exhausted = true;
            }
            matched.extend(
                rows.iter()
                    .filter_map(|d| decode_memory(d))
                    .filter(|m| matches(m)),
            );
            if exhausted {
                break;
            }
            offset += window;
            window *= 3;
        }
        let total = matched.len();
        matched.truncate(limit);
        let truncated = (!exhausted && scanned > 0 && (total < limit || total > matched.len()))
            .then_some(AudienceTruncation {
                sql_rows_examined: scanned,
                returned: matched.len(),
            });
        if let Some(t) = truncated
            && self.writable()
        {
            // The baseline audits the caller's context object as given. Here
            // the caller may be a read-scoped grant, so only the three
            // matching fields are recorded, each bounded: a read must not be
            // able to write arbitrary payloads into the store.
            let mut audited = serde_json::Map::new();
            for k in ["role", "taskType", "mode"] {
                if let Some(v) = context.get(k).and_then(Value::as_str) {
                    let bounded: String = v.chars().take(AUDIENCE_AUDIT_FIELD_MAX).collect();
                    audited.insert(k.to_string(), json!(bounded));
                }
            }
            crate::store::audit(
                &self.conn,
                "memory.audience_truncated",
                now_iso,
                json!({"context": audited, "sqlRowsExamined": t.sql_rows_examined, "returned": t.returned}),
            )
            .map_err(sql_err)?;
        }
        Ok(AudienceResult {
            memories: matched,
            truncated,
        })
    }
}

#[cfg(test)]
mod orphan_structure_tests {
    use super::*;
    use crate::MutationCtx;
    use wrongmemory_contracts::{MemoryAnchor, RememberParams};

    fn ctx(session: Option<&str>) -> MutationCtx {
        MutationCtx {
            principal_id: "host".into(),
            client_label: "t".into(),
            session_id: session.map(str::to_string),
            idempotency_key: None,
            now_iso: "2026-11-03T00:00:01.000Z".into(),
        }
    }

    fn anchor(kind: &str, path: &str, symbol: Option<&str>) -> MemoryAnchor {
        MemoryAnchor {
            kind: kind.into(),
            path: Some(path.into()),
            symbol: symbol.map(str::to_string),
            ..Default::default()
        }
    }

    /// WM-603 gap: retiring a memory removes its own edges but leaves the
    /// file/dir/symbol structure its anchors created. That orphan structure
    /// named a session-private memory's paths and symbols to every viewer.
    /// Structure no visible memory supports is now hidden; structure a
    /// visible memory supports stays.
    #[test]
    fn orphan_structure_of_a_retired_private_memory_is_not_disclosed() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("sage.db");
        SageStore::create_managed(&db, "2026-11-03T00:00:00.000Z").unwrap();
        let (mut store, _) = SageStore::open(&db, true, None, "2026-11-03T00:00:00.000Z").unwrap();
        let public = RememberParams {
            text: "The service entry point lives in src/app.rs.".into(),
            anchors: vec![anchor("file", "src/app.rs", None)],
            ..Default::default()
        };
        store.remember(&public, &ctx(None)).unwrap();
        let private = RememberParams {
            text: "Session scratch: orion vault key rotation notes.".into(),
            scope: Some("session".into()),
            anchors: vec![
                anchor("directory", "vault-orion/keys", None),
                anchor(
                    "symbol",
                    "vault-orion/keys/rotate.rs",
                    Some("rotate_orion_master"),
                ),
            ],
            ..Default::default()
        };
        let owner = ctx(Some("sess-a"));
        let id = store.remember(&private, &owner).unwrap().id;
        store
            .delete(&id, "done with it", true, false, &owner, true)
            .unwrap();

        for viewer in [
            Viewer::default(),
            Viewer {
                session_id: Some("sess-b".into()),
            },
            Viewer {
                session_id: Some("sess-a".into()),
            },
        ] {
            for query in [
                "vault-orion",
                "vault-orion/keys",
                "vault-orion/keys/rotate.rs",
                "dir:vault-orion",
                "symbol:vault-orion/keys/rotate.rs#rotate_orion_master",
            ] {
                let g = store.graph_for(query, 4, 100, &viewer).unwrap();
                let shown = serde_json::to_string(&g.edges).unwrap();
                assert!(
                    !shown.contains("vault-orion") && !shown.contains("rotate_orion"),
                    "{viewer:?} {query}: {shown}"
                );
            }
        }
        // Structure a visible memory supports is still traversed.
        let g = store
            .graph_for("src/app.rs", 2, 100, &Viewer::default())
            .unwrap();
        let shown = serde_json::to_string(&g.edges).unwrap();
        assert!(
            shown.contains("file:src/app.rs") && shown.contains("dir:src"),
            "{shown}"
        );
    }
}

#[cfg(test)]
mod audience_tests {
    use super::*;
    use crate::MutationCtx;
    use wrongmemory_contracts::{MemoryAudience, MemorySource, RememberParams};

    #[test]
    fn audience_truncation_audit_records_only_bounded_matching_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("sage.db");
        SageStore::create_managed(&db, "2026-11-01T00:00:00.000Z").unwrap();
        let (mut store, _) = SageStore::open(&db, true, None, "2026-11-01T00:00:00.000Z").unwrap();
        let role = "r".repeat(200);
        for w in [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf",
        ] {
            let p = RememberParams {
                text: format!("Distinct audience fact about {w} rotations"),
                audience: Some(MemoryAudience {
                    roles: Some(vec![role.clone()]),
                    ..Default::default()
                }),
                sources: vec![MemorySource {
                    kind: "user".into(),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let ctx = MutationCtx {
                principal_id: "host".into(),
                client_label: "t".into(),
                session_id: None,
                idempotency_key: None,
                now_iso: "2026-11-01T00:00:01.000Z".into(),
            };
            store.remember(&p, &ctx).unwrap();
        }
        let context = json!({"role": role, "extra": {"huge": "z".repeat(10_000)}});
        let res = store
            .retrieve_for_audience(
                &context,
                Some(1),
                &Viewer::default(),
                "2026-11-01T00:00:02.000Z",
            )
            .unwrap();
        assert_eq!(res.memories.len(), 1);
        assert!(res.truncated.is_some());
        let data: String = store
            .conn
            .query_row(
                "SELECT data FROM audit_log WHERE event = 'memory.audience_truncated'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(
            v["context"]["role"].as_str().unwrap().len(),
            AUDIENCE_AUDIT_FIELD_MAX
        );
        assert!(v["context"].get("extra").is_none());
        assert!(data.len() < 400, "audit row is {} bytes", data.len());
    }
}
