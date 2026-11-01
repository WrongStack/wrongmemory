//! WM-509: daemon-side fusion of lexical search with the semantic cache.
//!
//! A port of the baseline's `augmentLexicalWithVectorRecall`
//! (`retrieval/vector-augment.ts` at `222d23a`): RRF with k = 60 and vector
//! weight 0.3, a vector fetch of `max(2 * limit, 50)`, at most 12 vector-only
//! materializations, one rank sequence over the fusable vector hits, and no
//! extra vector-only tail. Differences, all on the side of not leaking:
//!
//! * Vector-only hits are materialized through the same SQL filter as the
//!   lexical search (session visibility, statuses, kinds, `contextPolicy`),
//!   so a hidden, retired or `never` record cannot enter through the vector
//!   channel; hidden reads like absent.
//! * A vector built from another revision than the current record is stale
//!   and does not count, for a boost or a materialization.
//! * The cosine floor for vector-only hits is the provider's own
//!   (`ProviderProfile::vector_only_threshold`).

use std::collections::{HashMap, HashSet};

use rusqlite::params;
use serde_json::json;
use wrongmemory_contracts::{SearchHit, SearchResult};

use crate::semantic::{EmbeddingProvider, SemanticCache, SourceRow, SyncReport};
use crate::store::{Filter, SageStore, StoreResult, Viewer, decode_memory, sql_err};

const RRF_K: f64 = 60.0;
const VECTOR_WEIGHT: f64 = 0.3;
const VECTOR_FETCH: usize = 50;
const MAX_MATERIALIZATIONS: usize = 12;

impl SageStore {
    /// Cheap change fingerprint of the active rows: any content, status or
    /// revision change moves it. Counter-only writes do not (WM-503: no
    /// embedding churn from usage).
    pub fn semantic_fingerprint(&self) -> StoreResult<String> {
        self.conn
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(updated_at), ''),
                        COALESCE(SUM(CASE WHEN json_valid(data)
                                 THEN json_extract(data, '$.revision') END), 0)
                   FROM memories WHERE status = 'active'",
                [],
                |r| {
                    Ok(format!(
                        "{}|{}|{}",
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?
                    ))
                },
            )
            .map_err(sql_err)
    }

    /// Every decodable active memory, for the semantic cache. Visibility is
    /// not applied here: it is applied when a vector hit is materialized.
    pub fn semantic_source_rows(&self) -> StoreResult<Vec<SourceRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM memories WHERE status = 'active'")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            if let Some(m) = decode_memory(&row.map_err(sql_err)?) {
                out.push(SourceRow {
                    id: m.id,
                    revision: m.revision,
                    text: m.text,
                });
            }
        }
        Ok(out)
    }

    /// Refresh `cache` from this store.
    pub fn sync_semantic(
        &self,
        cache: &mut SemanticCache,
        provider: &dyn EmbeddingProvider,
    ) -> StoreResult<SyncReport> {
        let fingerprint = self.semantic_fingerprint()?;
        cache.sync(&fingerprint, || self.semantic_source_rows(), provider)
    }

    /// Lexical search fused with the semantic channel (see the module docs).
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_search(
        &self,
        cache: &mut SemanticCache,
        provider: &dyn EmbeddingProvider,
        query: &str,
        limit: usize,
        kinds: Option<&[String]>,
        statuses: Option<&[String]>,
        viewer: &Viewer,
    ) -> StoreResult<SearchResult> {
        // Validates arguments and applies the lexical semantics unchanged.
        let lexical = self.search(query, limit, kinds, statuses, viewer)?;
        if query.trim().is_empty() {
            return Ok(lexical);
        }
        let limit = limit.clamp(1, wrongmemory_contracts::MAX_PAGE_SIZE);
        self.sync_semantic(cache, provider)?;
        let q = provider.embed(query)?;
        if q.is_empty() {
            return Ok(lexical);
        }
        let threshold = provider.profile().vector_only_threshold;
        let vector_hits = cache.search(&q, (limit * 2).max(VECTOR_FETCH), 0.0)?;

        let lexical_rev: HashMap<&str, i64> = lexical
            .hits
            .iter()
            .map(|h| (h.memory.id.as_str(), h.memory.revision))
            .collect();
        enum Kind {
            Boost,
            Materialize,
        }
        let mut classified = Vec::new();
        let mut seen = HashSet::new();
        let mut budget = MAX_MATERIALIZATIONS;
        for hit in &vector_hits {
            if !seen.insert(hit.id.as_str()) {
                continue;
            }
            match lexical_rev.get(hit.id.as_str()) {
                Some(rev) if *rev == hit.revision => classified.push((hit, Kind::Boost)),
                Some(_) => {} // stale vector of a lexical hit: no boost
                None if hit.score >= threshold && budget > 0 => {
                    budget -= 1;
                    classified.push((hit, Kind::Materialize));
                }
                None => {}
            }
        }

        // Materialize vector-only ids under the lexical search's own filter.
        let ids: Vec<&str> = classified
            .iter()
            .filter(|(_, k)| matches!(k, Kind::Materialize))
            .map(|(h, _)| h.id.as_str())
            .collect();
        let mut materialized = HashMap::new();
        if !ids.is_empty() {
            let default_status = ["active".to_string()];
            let explicit = statuses.is_some_and(|s| !s.is_empty());
            let filter = Filter {
                statuses: statuses
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&default_status),
                kinds,
                exclude_never: !explicit,
                viewer,
            };
            let placeholders = vec!["?"; ids.len()].join(", ");
            let found = self.select_hits(
                &filter,
                &format!("m.id IN ({placeholders})"),
                ids.iter().map(|id| json!(id)).collect(),
                "",
                ids.len(),
                "vector",
            )?;
            for h in found.hits {
                materialized.insert(h.memory.id.clone(), h.memory);
            }
        }

        // RRF over both channels.
        let mut fused: Vec<(SearchHit, f64)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for (i, hit) in lexical.hits.iter().enumerate() {
            index.insert(hit.memory.id.clone(), fused.len());
            fused.push((
                hit.clone(),
                (1.0 - VECTOR_WEIGHT) * (1.0 / (RRF_K + i as f64 + 1.0)),
            ));
        }
        let mut rank = 0usize;
        for (hit, kind) in classified {
            let rrf = VECTOR_WEIGHT * (1.0 / (RRF_K + rank as f64 + 1.0));
            match kind {
                Kind::Boost => {
                    let (h, score) = &mut fused[index[&hit.id]];
                    *score += rrf;
                    h.channel = "both".into();
                    h.reasons.push(format!("vector cosine {:.3}", hit.score));
                }
                Kind::Materialize => {
                    let Some(memory) = materialized.remove(&hit.id) else {
                        continue; // hidden, retired or absent: no rank consumed
                    };
                    if memory.revision != hit.revision {
                        continue; // stale vector
                    }
                    fused.push((
                        SearchHit {
                            memory,
                            channel: "vector".into(),
                            score: 0.0,
                            reasons: vec![format!("vector cosine {:.3}", hit.score)],
                        },
                        rrf,
                    ));
                }
            }
            rank += 1;
        }
        fused.sort_by(|a, b| b.1.total_cmp(&a.1));
        let truncated = lexical.truncated || fused.len() > limit;
        fused.truncate(limit);
        Ok(SearchResult {
            hits: fused
                .into_iter()
                .map(|(mut h, score)| {
                    h.score = score;
                    h
                })
                .collect(),
            truncated,
            corrupt_skipped: lexical.corrupt_skipped,
            channel: "hybrid".into(),
        })
    }
}
