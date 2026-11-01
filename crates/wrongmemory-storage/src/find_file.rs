//! `findMemoriesForFile` (baseline `sqlite-store-find-file.ts`): memories
//! about one file, bucketed into primary (file/scope/directory anchors),
//! symbol and mention matches, with review and supersession hints.
//!
//! Visibility-safe: memories are read under the session rule, and the
//! supersession head and pending reviews are looked up among visible
//! records only (the baseline resolves both against every row, which would
//! hand out the ids of other sessions' private records).

use std::collections::HashMap;

use serde_json::{Value, json};
use wrongmemory_contracts::{ErrorBody, ErrorCode, MemoryAnchor, MemoryCandidate, SageRecord};

use crate::paths::normalize_project_path;
use crate::store::{SageStore, StoreResult, Viewer, decode_memory, sql_err};
use crate::text::{normalize_slashes, parse_iso_ms};

#[derive(Debug, Clone, Default)]
pub struct FileQuery {
    pub include_superseded: Option<bool>,
    pub include_deleted: Option<bool>,
    pub limit: Option<i64>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
}

fn anchor_line(a: &MemoryAnchor, key: &str) -> Option<i64> {
    a.extra.get(key).and_then(Value::as_i64)
}

struct Matcher<'a> {
    root: &'a std::path::Path,
    target: String,
    basename: String,
    line_start: Option<i64>,
    line_end: Option<i64>,
    cache: HashMap<String, Option<String>>,
}

impl Matcher<'_> {
    /// `anchorPath`: project-relative anchor path, memoized per call.
    fn anchor_path(&mut self, value: &str) -> Option<String> {
        if let Some(hit) = self.cache.get(value) {
            return hit.clone();
        }
        let resolved = match normalize_project_path(self.root, value) {
            Ok(p) => Some(p),
            Err(_) => {
                let n = normalize_slashes(value);
                let n = n.trim_start_matches("./").to_string();
                (!n.starts_with("../")).then_some(n)
            }
        };
        self.cache.insert(value.to_string(), resolved.clone());
        resolved
    }

    /// `matchMemory`.
    fn matches(&mut self, m: &SageRecord) -> Option<(&'static str, f64)> {
        let mut directory = None;
        for a in &m.anchors {
            let Some(p) = a.path.as_deref() else { continue };
            let Some(normalized) = self.anchor_path(p) else {
                continue;
            };
            if normalized == self.target {
                if a.kind == "symbol" {
                    let overlaps = match (
                        self.line_start,
                        self.line_end,
                        anchor_line(a, "lineStart"),
                        anchor_line(a, "lineEnd"),
                    ) {
                        (Some(s), Some(e), Some(a_s), Some(a_e)) => s <= a_e && e >= a_s,
                        _ => false,
                    };
                    return Some(if m.scope == "symbol" {
                        ("scope_symbol", if overlaps { 1.0 } else { 0.92 })
                    } else {
                        ("anchor_symbol", if overlaps { 0.95 } else { 0.75 })
                    });
                }
                return Some(if m.scope == "file" {
                    ("scope_file", 1.0)
                } else {
                    ("anchor_file", 0.9)
                });
            }
            if matches!(a.kind.as_str(), "directory" | "package")
                && (normalized == "." || self.target.starts_with(&format!("{normalized}/")))
            {
                directory.get_or_insert(("anchor_directory", 0.5));
            }
        }
        if directory.is_some() {
            return directory;
        }
        if !self.basename.is_empty()
            && self.basename != "."
            && m.text
                .to_lowercase()
                .contains(&self.basename.to_lowercase())
        {
            return Some(("mention", 0.3));
        }
        None
    }
}

impl SageStore {
    pub fn find_memories_for_file(
        &self,
        file_path: &str,
        q: &FileQuery,
        viewer: &Viewer,
        now_iso: &str,
    ) -> StoreResult<Value> {
        let root = self.project_root.clone().ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::NotImplemented,
                "this store has no project root for file lookups",
            )
        })?;
        let target = normalize_project_path(&root, file_path)
            .map_err(|e| ErrorBody::new(ErrorCode::InvalidRequest, e))?;
        let basename = if target == "." {
            String::new()
        } else {
            target.rsplit('/').next().unwrap_or("").to_string()
        };
        let include_superseded = q.include_superseded != Some(false);
        let include_deleted = q.include_deleted == Some(true);
        let limit = q.limit.map_or(50, |l| l.clamp(0, 250)).max(1) as usize;

        // listMemories (updated_at DESC), visible rows only.
        let mut args: Vec<rusqlite::types::Value> = vec![viewer.session_id.clone().into()];
        let clause = crate::store::session_clause("", viewer, 1);
        let memories: Vec<SageRecord> = self
            .conn
            .prepare(&format!(
                "SELECT data FROM memories WHERE {clause} ORDER BY updated_at DESC"
            ))
            .map_err(sql_err)?
            .query_map(rusqlite::params_from_iter(args.drain(..)), |r| {
                r.get::<_, String>(0)
            })
            .map_err(sql_err)?
            .filter_map(|r| r.ok().and_then(|d| decode_memory(&d)))
            .collect();
        let by_id: HashMap<&str, &SageRecord> =
            memories.iter().map(|m| (m.id.as_str(), m)).collect();

        // Pending reviews (updated_at DESC; later entries win, as Map.set).
        let now_ms = parse_iso_ms(now_iso).unwrap_or(0);
        let mut pending: HashMap<String, Value> = HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT data FROM candidates WHERE status = 'pending'
                       AND (NOT json_valid(data)
                            OR json_extract(data, '$.scope') IS NOT 'session'
                            OR (json_extract(data, '$.ownerSessionId') IS NOT NULL
                                AND json_extract(data, '$.ownerSessionId') = ?1))
                     ORDER BY updated_at DESC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([&viewer.session_id], |r| r.get::<_, String>(0))
                .map_err(sql_err)?;
            for d in rows.flatten() {
                let Ok(c) = serde_json::from_str::<MemoryCandidate>(&d) else {
                    continue;
                };
                let Some(source) = c.target_memory_id.clone().or(c.memory_id.clone()) else {
                    continue;
                };
                if !by_id.contains_key(source.as_str()) {
                    continue;
                }
                let action = c
                    .suggested_action
                    .as_deref()
                    .filter(|a| matches!(*a, "delete" | "archive" | "update"))
                    .unwrap_or("investigate");
                let age = parse_iso_ms(&c.created_at)
                    .map(|t| ((now_ms - t) as f64 / 86_400_000.0).floor().max(0.0) as i64)
                    .unwrap_or(0);
                pending.insert(
                    source,
                    json!({
                        "candidateId": c.id,
                        "reason": c.review_reason.clone().unwrap_or(c.kind.clone()),
                        "suggestedAction": action,
                        "ageDays": age,
                    }),
                );
            }
        }

        let mut matcher = Matcher {
            root: &root,
            target: target.clone(),
            basename,
            line_start: q.line_start,
            line_end: q.line_end,
            cache: HashMap::new(),
        };
        let mut primary: Vec<(f64, String, Value)> = Vec::new();
        let mut symbol: Vec<(f64, String, Value)> = Vec::new();
        let mut related: Vec<(f64, String, Value)> = Vec::new();
        for m in &memories {
            if m.status == "deleted" && !include_deleted {
                continue;
            }
            if m.status == "superseded" && !include_superseded {
                continue;
            }
            let Some((via, strength)) = matcher.matches(m) else {
                continue;
            };
            let mut entry = json!({
                "memory": m,
                "matchedVia": via,
                "matchStrength": strength,
            });
            if m.status == "superseded"
                && let Some(head) = m.superseded_by.as_deref().and_then(|h| by_id.get(h))
                && head.status == "active"
            {
                entry["supersededByActiveId"] = json!(head.id);
            }
            if let Some(p) = pending.get(&m.id) {
                entry["pendingReview"] = p.clone();
            }
            let item = (strength, m.updated_at.clone(), entry);
            match via {
                "scope_symbol" | "anchor_symbol" => symbol.push(item),
                "mention" => related.push(item),
                _ => primary.push(item),
            }
        }
        // strength DESC, then updatedAt DESC (byte order); stable.
        for bucket in [&mut primary, &mut symbol, &mut related] {
            bucket.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        }
        let take = |v: Vec<(f64, String, Value)>, n: usize| -> Vec<Value> {
            v.into_iter().take(n).map(|(_, _, e)| e).collect()
        };
        let primary = take(primary, limit);
        let symbol = take(symbol, limit.saturating_sub(primary.len()));
        let related = take(related, limit.saturating_sub(primary.len() + symbol.len()));
        let all: Vec<&Value> = primary.iter().chain(&symbol).chain(&related).collect();
        let count_status = |s: &str| all.iter().filter(|e| e["memory"]["status"] == s).count();
        Ok(json!({
            "filePath": target,
            "totalCount": all.len(),
            "activeCount": count_status("active"),
            "supersededCount": count_status("superseded"),
            "reviewPendingCount": all.iter().filter(|e| e.get("pendingReview").is_some()).count(),
            "primaryMatches": primary,
            "symbolMatches": symbol,
            "relatedMatches": related,
        }))
    }
}
