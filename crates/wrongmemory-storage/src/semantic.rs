//! WM-508/509: optional semantic recall behind a service-owned provider
//! boundary (WM-ADR-008, retrieval-and-lifecycle.md §5).
//!
//! * [`EmbeddingProvider`] is the provider interface. Lexical retrieval never
//!   depends on it: with no provider configured every search stays lexical,
//!   and an explicit semantic request fails with `SEMANTIC_UNAVAILABLE`
//!   instead of fabricating a zero-hit answer.
//! * [`NgramHashProvider`] is the one built-in provider: hashed character
//!   n-grams (sub-word similarity), deterministic, offline, no model file, no
//!   new dependency. It is not a neural embedding model; it recovers matches
//!   the lexical term split cannot see (CJK substrings, inflected and
//!   compound words, small typos). A neural model is WM-805 and needs its own
//!   licence/compatibility investigation.
//! * [`SemanticCache`] is the derived, rebuildable vector cache. It lives in
//!   its own SQLite file outside the SAGE store (SAGE rows stay authoritative,
//!   WM-INV-04/15), records the provider profile, memory id, revision and a
//!   text hash per vector, and is searched by bounded exact cosine. Stale
//!   vectors are excluded at materialization by revision (see
//!   `SageStore::hybrid_search`), even before the next sync rewrites them.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use wrongmemory_contracts::{ErrorBody, ErrorCode};

use crate::text::unicode_fold;

/// A sparse, L2-normalized vector: `(dimension, weight)` sorted by dimension.
pub type SparseVector = Vec<(u32, f32)>;

/// Identity of the vectors a provider produces. Vectors from different
/// profiles are never compared (model partitioning).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    pub provider: String,
    pub model: String,
    pub dims: u32,
    pub preprocessing: String,
    /// Cosine floor for admitting a vector-only hit (one the lexical channel
    /// did not find). Provider-specific: the baseline's 0.62 is a
    /// MiniLM-class value, not a universal constant.
    pub vector_only_threshold: f64,
}

impl ProviderProfile {
    /// Stable key used to partition the cache.
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.provider, self.model, self.dims, self.preprocessing
        )
    }
}

/// The provider boundary. Implementations must be deterministic for a given
/// profile, or the cache's revision/hash bookkeeping is meaningless.
pub trait EmbeddingProvider: Send + Sync {
    fn profile(&self) -> &ProviderProfile;
    fn embed(&self, text: &str) -> Result<SparseVector, ErrorBody>;
}

pub const PROVIDER_NONE: &str = "none";
pub const PROVIDER_NGRAM_HASH: &str = "ngram-hash";
/// Providers this build can construct.
pub const PROVIDERS: &[&str] = &[PROVIDER_NONE, PROVIDER_NGRAM_HASH];

/// Construct a provider by name; `none` (and unknown names) yield `None`.
pub fn provider_for(name: &str) -> Option<Box<dyn EmbeddingProvider>> {
    match name {
        PROVIDER_NGRAM_HASH => Some(Box::new(NgramHashProvider::new())),
        _ => None,
    }
}

pub fn semantic_unavailable(message: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::SemanticUnavailable, message)
}

const NGRAM_DIM_BITS: u32 = 20;
/// Longest text embedded, in characters; longer texts use their prefix.
const MAX_EMBED_CHARS: usize = 20_000;

/// Hashed character n-grams (2..=4 over each boundary-marked word) plus
/// whole-word features, sublinear term frequency, L2 normalization, hashed
/// into 2^20 dimensions with FNV-1a.
pub struct NgramHashProvider {
    profile: ProviderProfile,
}

impl NgramHashProvider {
    pub fn new() -> Self {
        Self {
            profile: ProviderProfile {
                provider: PROVIDER_NGRAM_HASH.into(),
                model: "char2-4-word-fnv1a".into(),
                dims: 1 << NGRAM_DIM_BITS,
                preprocessing: "nfkc-lower-v1".into(),
                vector_only_threshold: NGRAM_VECTOR_ONLY_THRESHOLD,
            },
        }
    }
}

impl Default for NgramHashProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Chosen on the WM-605 and WM-007 corpora (2026-11-01): the weakest
/// target cosine was 0.195 (an unspaced Japanese substring query), while
/// unrelated English records reach 0.6 through shared function-word n-grams,
/// so no floor separates them by cosine alone; RRF at weight 0.3 is what
/// keeps that noise below strong lexical hits. 0.20 lost the Japanese target;
/// 0.15 and 0.10 measured the same (16/16, English MRR 0.988), and 0.15 is
/// the more conservative. Small corpora: revisit with more evidence, not by
/// analogy with MiniLM's 0.62.
pub const NGRAM_VECTOR_ONLY_THRESHOLD: f64 = 0.15;

fn fnv1a(tag: u8, s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in std::iter::once(tag).chain(s.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

impl EmbeddingProvider for NgramHashProvider {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    fn embed(&self, text: &str) -> Result<SparseVector, ErrorBody> {
        let text: String = text.chars().take(MAX_EMBED_CHARS).collect();
        let folded = unicode_fold(&text);
        let mask = (1u64 << NGRAM_DIM_BITS) - 1;
        let mut tf: HashMap<u32, f32> = HashMap::new();
        for word in folded
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
        {
            *tf.entry((fnv1a(b'w', word) & mask) as u32).or_default() += 1.0;
            let chars: Vec<char> = std::iter::once('\u{2}')
                .chain(word.chars())
                .chain(std::iter::once('\u{3}'))
                .collect();
            for n in 2..=4 {
                for gram in chars.windows(n) {
                    let g: String = gram.iter().collect();
                    *tf.entry((fnv1a(b'0' + n as u8, &g) & mask) as u32)
                        .or_default() += 1.0;
                }
            }
        }
        let mut v: SparseVector = tf.into_iter().map(|(i, c)| (i, 1.0 + c.ln())).collect();
        v.sort_unstable_by_key(|(i, _)| *i);
        normalize(&mut v);
        Ok(v)
    }
}

fn normalize(v: &mut SparseVector) {
    let norm = v.iter().map(|(_, w)| w * w).sum::<f32>().sqrt();
    if norm > 0.0 {
        for (_, w) in v.iter_mut() {
            *w /= norm;
        }
    }
}

/// Cosine of two normalized sparse vectors.
pub fn cosine(a: &[(u32, f32)], b: &[(u32, f32)]) -> f64 {
    let (mut i, mut j, mut dot) = (0, 0, 0.0f64);
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                dot += f64::from(a[i].1) * f64::from(b[j].1);
                i += 1;
                j += 1;
            }
        }
    }
    dot
}

fn encode(v: &[(u32, f32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 8);
    for (i, w) in v {
        out.extend_from_slice(&i.to_le_bytes());
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

fn decode(b: &[u8]) -> Option<SparseVector> {
    if !b.len().is_multiple_of(8) {
        return None;
    }
    Some(
        b.chunks_exact(8)
            .map(|c| {
                (
                    u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect(),
    )
}

pub fn text_hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// One active memory as the cache sees it.
pub struct SourceRow {
    pub id: String,
    pub revision: i64,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub embedded: usize,
    pub removed: usize,
    pub indexed: usize,
    /// Nothing changed since the last sync (fingerprint matched).
    pub unchanged: bool,
}

/// A cached vector hit: memory id, the revision the vector was built from,
/// and the cosine with the query.
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: String,
    pub revision: i64,
    pub score: f64,
}

const CACHE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS cache_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS vectors (
  memory_id TEXT NOT NULL,
  profile TEXT NOT NULL,
  revision INTEGER NOT NULL,
  text_hash TEXT NOT NULL,
  vector BLOB NOT NULL,
  PRIMARY KEY (memory_id, profile)
);
"#;

/// The derived semantic cache of one project.
pub struct SemanticCache {
    conn: Connection,
    profile: ProviderProfile,
}

impl SemanticCache {
    /// Open (or create) the cache at `path` for `profile`. Vectors of any
    /// other profile are dropped: the cache holds one partition at a time.
    pub fn open(path: &Path, profile: &ProviderProfile) -> Result<Self, ErrorBody> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io_err)?;
        }
        let conn = Connection::open(path).map_err(cache_err)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
            .map_err(cache_err)?;
        conn.execute_batch(CACHE_SCHEMA).map_err(cache_err)?;
        let key = profile.key();
        conn.execute("DELETE FROM vectors WHERE profile <> ?1", [&key])
            .map_err(cache_err)?;
        let stored: Option<String> = conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key = 'profile'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(cache_err)?;
        if stored.as_deref() != Some(key.as_str()) {
            conn.execute(
                "INSERT OR REPLACE INTO cache_meta(key, value) VALUES ('profile', ?1)",
                [&key],
            )
            .map_err(cache_err)?;
            conn.execute("DELETE FROM cache_meta WHERE key = 'fingerprint'", [])
                .map_err(cache_err)?;
        }
        Ok(Self {
            conn,
            profile: profile.clone(),
        })
    }

    pub fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    pub fn indexed(&self) -> Result<usize, ErrorBody> {
        self.conn
            .query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .map_err(cache_err)
    }

    /// Bring the cache in line with `rows` (every active memory). Skipped
    /// when `fingerprint` matches the last sync.
    pub fn sync(
        &mut self,
        fingerprint: &str,
        rows: impl FnOnce() -> Result<Vec<SourceRow>, ErrorBody>,
        provider: &dyn EmbeddingProvider,
    ) -> Result<SyncReport, ErrorBody> {
        if provider.profile().key() != self.profile.key() {
            return Err(semantic_unavailable(
                "the semantic cache belongs to another provider profile",
            ));
        }
        let last: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key = 'fingerprint'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(cache_err)?;
        if last.as_deref() == Some(fingerprint) {
            return Ok(SyncReport {
                indexed: self.indexed()?,
                unchanged: true,
                ..Default::default()
            });
        }
        let rows = rows()?;
        let mut existing: HashMap<String, (i64, String)> = HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT memory_id, revision, text_hash FROM vectors")
                .map_err(cache_err)?;
            let it = stmt
                .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))
                .map_err(cache_err)?;
            for row in it {
                let (id, v): (String, (i64, String)) = row.map_err(cache_err)?;
                existing.insert(id, v);
            }
        }
        let key = self.profile.key();
        let tx = self.conn.transaction().map_err(cache_err)?;
        let mut report = SyncReport::default();
        for row in &rows {
            let hash = text_hash(&row.text);
            let fresh = existing
                .remove(&row.id)
                .is_some_and(|(rev, h)| rev == row.revision && h == hash);
            if fresh {
                continue;
            }
            let v = provider.embed(&row.text)?;
            tx.execute(
                "INSERT OR REPLACE INTO vectors(memory_id, profile, revision, text_hash, vector)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![row.id, key, row.revision, hash, encode(&v)],
            )
            .map_err(cache_err)?;
            report.embedded += 1;
        }
        for id in existing.keys() {
            tx.execute("DELETE FROM vectors WHERE memory_id = ?1", [id])
                .map_err(cache_err)?;
            report.removed += 1;
        }
        tx.execute(
            "INSERT OR REPLACE INTO cache_meta(key, value) VALUES ('fingerprint', ?1)",
            [fingerprint],
        )
        .map_err(cache_err)?;
        tx.commit().map_err(cache_err)?;
        report.indexed = rows.len();
        Ok(report)
    }

    /// Bounded exact cosine search: the `fetch` best vectors at or above
    /// `threshold`, best first (ties by id for determinism).
    pub fn search(
        &self,
        query: &[(u32, f32)],
        fetch: usize,
        threshold: f64,
    ) -> Result<Vec<VectorHit>, ErrorBody> {
        let mut stmt = self
            .conn
            .prepare("SELECT memory_id, revision, vector FROM vectors WHERE profile = ?1")
            .map_err(cache_err)?;
        let rows = stmt
            .query_map([self.profile.key()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(cache_err)?;
        let mut hits = Vec::new();
        for row in rows {
            let (id, revision, blob) = row.map_err(cache_err)?;
            // A corrupt vector is skipped; the next sync rewrites it only if
            // its revision or text changed, so drop it here to be safe.
            let Some(v) = decode(&blob) else { continue };
            let score = cosine(query, &v);
            if score > 0.0 && score >= threshold {
                hits.push(VectorHit {
                    id,
                    revision,
                    score,
                });
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(fetch);
        Ok(hits)
    }
}

fn cache_err(e: rusqlite::Error) -> ErrorBody {
    semantic_unavailable(format!("semantic cache: {e}"))
}

fn io_err(e: std::io::Error) -> ErrorBody {
    semantic_unavailable(format!("semantic cache: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> NgramHashProvider {
        NgramHashProvider::new()
    }

    #[test]
    fn vectors_are_normalized_deterministic_and_sorted() {
        let a = p().embed("Café caches are flushed").unwrap();
        let b = p().embed("Café caches are flushed").unwrap();
        assert_eq!(a, b);
        assert!(a.windows(2).all(|w| w[0].0 < w[1].0));
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-5);
        assert!(p().embed("").unwrap().is_empty());
    }

    #[test]
    fn subword_overlap_scores_above_unrelated_text() {
        let doc = p()
            .embed("テストは並列実行されるので共有状態を使わないこと。")
            .unwrap();
        let q = p().embed("並列実行").unwrap();
        let other = p().embed("生产环境的日志保留九十天。").unwrap();
        assert!(cosine(&q, &doc) > cosine(&q, &other));
        let de = p().embed("Konfigurationsdateien werden geprüft").unwrap();
        assert!(cosine(&p().embed("Konfiguration").unwrap(), &de) > 0.1);
    }

    #[test]
    fn cache_syncs_incrementally_and_partitions_by_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache").join("p.sqlite");
        let provider = p();
        let mut cache = SemanticCache::open(&path, provider.profile()).unwrap();
        let rows = |texts: &[(&str, i64, &str)]| {
            texts
                .iter()
                .map(|(id, rev, t)| SourceRow {
                    id: (*id).into(),
                    revision: *rev,
                    text: (*t).into(),
                })
                .collect::<Vec<_>>()
        };
        let r = cache
            .sync(
                "f1",
                || Ok(rows(&[("a", 1, "alpha beta"), ("b", 1, "gamma")])),
                &provider,
            )
            .unwrap();
        assert_eq!((r.embedded, r.removed, r.indexed), (2, 0, 2));
        let same = cache.sync("f1", || panic!("not read"), &provider).unwrap();
        assert!(same.unchanged);
        let r = cache
            .sync(
                "f2",
                || Ok(rows(&[("a", 2, "alpha beta delta")])),
                &provider,
            )
            .unwrap();
        assert_eq!((r.embedded, r.removed, r.indexed), (1, 1, 1));
        let hits = cache
            .search(&provider.embed("delta").unwrap(), 10, 0.0)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].id.as_str(), hits[0].revision), ("a", 2));
        // Another profile never sees these vectors.
        let mut other = provider.profile().clone();
        other.model = "other".into();
        drop(cache);
        let cache = SemanticCache::open(&path, &other).unwrap();
        assert_eq!(cache.indexed().unwrap(), 0);
    }
}
