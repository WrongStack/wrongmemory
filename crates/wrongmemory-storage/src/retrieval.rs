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

fn escape_glob(s: &str) -> String {
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
        let mut globs: Vec<String> = Vec::new();
        let mut add = |t: String| {
            if !targets.contains(&t) {
                targets.push(t);
            }
        };
        for p in &rel {
            add(format!("file:{p}"));
            globs.push(format!("symbol:{}#*", escape_glob(p)));
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
        let glob_clause: Vec<String> = globs
            .iter()
            .map(|g| {
                args.push(g.clone().into());
                format!("e.to_node GLOB ?{}", args.len())
            })
            .collect();
        let status_ph = placeholders(&mut args, &statuses);
        let session = session_sql("m.", viewer, &mut args);
        let sql = format!(
            "SELECT DISTINCT m.data FROM memories m
              WHERE m.id IN (
                SELECT SUBSTR(e.from_node, 5) FROM edges e
                 WHERE e.from_node GLOB 'mem:*' AND (e.to_node IN ({target_ph}) OR {}))
                AND m.status IN ({status_ph}) AND {session}{}
              ORDER BY m.importance DESC, m.updated_at DESC LIMIT {}",
            glob_clause.join(" OR "),
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
            n.strip_prefix("mem:").is_some_and(|id| hidden.contains(id))
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
            for a in &h.memory.anchors {
                if let Some(n) = crate::store::anchor_node(a) {
                    add(n);
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

    /// `findRelatedSage`: graph neighbours plus anchor/tag overlap, scored
    /// with `scoreMemoryRelationship`. Candidate collection is a bounded
    /// approximation of `collectRelatedSqliteCandidateIds`.
    pub fn find_related(
        &self,
        ids: &[String],
        limit: usize,
        max_depth: usize,
        viewer: &Viewer,
    ) -> StoreResult<Vec<SearchHit>> {
        let limit = limit.clamp(1, wrongmemory_contracts::MAX_PAGE_SIZE);
        let mut seeds = Vec::new();
        for id in ids {
            if let Some(m) = self.get(id, viewer)? {
                seeds.push(m);
            }
        }
        if seeds.is_empty() {
            return Ok(vec![]);
        }
        let budget = (limit * 20).max(100);
        let starts: Vec<String> = seeds.iter().map(|s| format!("mem:{}", s.id)).collect();
        let (edges, _) = self.traverse(&starts, max_depth.max(1), budget, viewer)?;
        let graph: HashSet<String> = edges
            .iter()
            .flat_map(|e| [&e.from, &e.to])
            .filter_map(|n| n.strip_prefix("mem:"))
            .map(str::to_string)
            .collect();
        let mut cand: Vec<String> = graph.iter().cloned().collect();
        let mut targets: Vec<String> = Vec::new();
        for s in &seeds {
            for a in &s.anchors {
                if let Some(n) = crate::store::anchor_node(a) {
                    targets.push(n);
                }
            }
        }
        if !targets.is_empty() {
            let mut args = Vec::new();
            let ph = placeholders(&mut args, &targets);
            for id in self.query_data(
                &format!("SELECT DISTINCT SUBSTR(from_node, 5) FROM edges WHERE from_node GLOB 'mem:*' AND to_node IN ({ph}) LIMIT {budget}"),
                args,
            )? {
                cand.push(id);
            }
        }
        let tags: Vec<String> = seeds.iter().flat_map(|s| s.tags.clone()).collect();
        for t in tags.iter().take(16) {
            for id in self.query_data(
                &format!("SELECT m.id FROM memories m, json_each(m.tags) t WHERE json_valid(m.tags) AND t.value = ?1 LIMIT {budget}"),
                vec![t.clone().into()],
            )? {
                cand.push(id);
            }
        }
        cand.sort();
        cand.dedup();
        let seed_refs: Vec<&SageRecord> = seeds.iter().collect();
        let mut hits = Vec::new();
        for id in cand {
            if let Some(m) = self.get(&id, viewer)?
                && m.status == "active"
            {
                let score = crate::similarity::score_relationship(&m, &seed_refs, &graph);
                if score > 0.0 {
                    hits.push(SearchHit {
                        memory: m,
                        channel: "related".into(),
                        score,
                        reasons: vec![],
                    });
                }
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
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
        let pending: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM candidates WHERE status = 'pending'",
                [],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        let edges: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
            .map_err(sql_err)?;
        Ok(
            json!({"byStatus": counts, "byKind": by_kind, "pendingCandidates": pending, "edges": edges}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
