//! SAGE store handle: visibility-bound reads and the M1 native write slice.
//!
//! Scope of this module (see docs/status.md): exact-ID read, lexical search
//! (FTS5 with LIKE fallback), path retrieval over anchors, candidate list,
//! `remember` (new record + anchor edges + audit + receipt) and
//! `propose` (pending candidate + receipt). SAGE's remember-time quality caps,
//! near-duplicate merge, contradiction detection, update/delete/recovery and
//! hygiene are NOT ported yet (M5, WM-500..511) and are not claimed.
//!
//! Visibility (WM-INV-08) is applied in SQL before LIMIT: a session-scoped
//! row is visible only to the session that owns it; with no bound session all
//! session-scoped rows are hidden. Hidden and absent IDs are indistinguishable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use wrongmemory_contracts::{
    ANCHOR_TYPES, CANDIDATE_STATUSES, ErrorBody, ErrorCode, MAX_METADATA_ITEMS, MAX_PAGE_SIZE,
    MAX_TEXT_CHARS, MAX_VALIDITY_CHECKS, MemoryAnchor, MemoryCandidate, MutationOutcome,
    ProposeParams, RememberParams, SAGE_KINDS, SAGE_SCOPES, SAGE_STATUSES, SUGGESTED_ACTIONS,
    SageRecord, SearchHit, SearchResult,
};

use crate::inspect::{self, Inspection};
use crate::schema;
use crate::text::{
    ancestor_paths, js_length, normalize_slashes, normalize_text, normalize_text_key,
    posix_dirname, unicode_fold,
};

/// SAGE audit trail bound (`AUDIT_LOG_MAX_ROWS` / `AUDIT_LOG_PRUNE_INTERVAL`).
const AUDIT_LOG_MAX_ROWS: i64 = 1000;
const AUDIT_LOG_PRUNE_INTERVAL: i64 = 256;
/// LIKE fallback never scans more than this many visible rows.
const FALLBACK_SCAN_LIMIT: usize = 5000;

pub type StoreResult<T> = Result<T, ErrorBody>;

/// Map SQLite failures to typed errors, keeping the primary message.
pub fn sql_err(e: rusqlite::Error) -> ErrorBody {
    use rusqlite::ErrorCode as C;
    let code = match e.sqlite_error_code() {
        Some(C::DatabaseBusy | C::DatabaseLocked) => ErrorCode::DatabaseBusy,
        Some(C::DiskFull) => ErrorCode::DiskFull,
        Some(C::DatabaseCorrupt | C::NotADatabase) => ErrorCode::DatabaseCorrupt,
        Some(C::ReadOnly) => ErrorCode::ReadOnly,
        _ => ErrorCode::Internal,
    };
    ErrorBody::new(code, e.to_string())
}

fn invalid(msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::InvalidRequest, msg)
}

/// The caller's trusted read context. Built by the daemon from the
/// authenticated connection, never from request parameters.
#[derive(Debug, Clone, Default)]
pub struct Viewer {
    pub session_id: Option<String>,
}

/// Trusted mutation context (principal and session are daemon-bound).
#[derive(Debug, Clone)]
pub struct MutationCtx {
    pub principal_id: String,
    pub client_label: String,
    pub session_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub now_iso: String,
}

pub struct SageStore {
    pub(crate) conn: Connection,
    path: PathBuf,
    writable: bool,
    fts: bool,
    unicode_text: bool,
    pub(crate) project_root: Option<PathBuf>,
}

impl SageStore {
    /// Open an inspected store. Read-only handles never run DDL. Writable
    /// handles require a `Compatible` verdict and apply the additive
    /// extension once; the caller must already hold the [`crate::OwnerGuard`].
    pub fn open(
        path: &Path,
        writable: bool,
        project_root: Option<PathBuf>,
        now_iso: &str,
    ) -> StoreResult<(Self, Inspection)> {
        let inspection = inspect::inspect(path);
        if !inspection.verdict.readable() {
            return Err(ErrorBody::new(
                ErrorCode::ProfileUnsupported,
                format!("{:?}: {}", inspection.verdict, inspection.detail),
            ));
        }
        if writable && !inspection.verdict.writable() {
            return Err(ErrorBody::new(
                ErrorCode::ProfileUnsupported,
                format!(
                    "store is readable but not writable by this build: {}",
                    inspection.detail
                ),
            ));
        }
        let conn = if writable {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(sql_err)?;
            conn.execute_batch(schema::CONNECTION_PRAGMAS)
                .map_err(sql_err)?;
            if inspection.wrongmemory_extension.is_none() {
                schema::apply_extension(&conn, now_iso).map_err(sql_err)?;
            }
            conn
        } else {
            inspect::open_read_only(path).map_err(sql_err)?
        };
        conn.busy_timeout(Duration::from_secs(30))
            .map_err(sql_err)?;
        // Fault injection (debug builds only, `WRONGMEMORY_FAULT=disk_full`):
        // cap the file at its current size so the next growing write fails
        // with a real SQLITE_FULL (WM-208).
        if writable && fault_is("disk_full") {
            let pages: i64 = conn
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .map_err(sql_err)?;
            conn.pragma_update(None, "max_page_count", pages)
                .map_err(sql_err)?;
        }
        Ok((
            Self {
                conn,
                path: path.to_path_buf(),
                writable,
                fts: inspection.fts_present,
                unicode_text: inspection.unicode_text_column,
                project_root,
            },
            inspection,
        ))
    }

    /// Create a new managed SAGE v5 store (plus `manifest.json`) at `path`.
    /// Refuses to touch an existing file.
    pub fn create_managed(path: &Path, now_iso: &str) -> StoreResult<()> {
        if path.exists() {
            return Err(ErrorBody::new(
                ErrorCode::StoreIdentityConflict,
                format!(
                    "{} already exists; refusing to initialize over it",
                    path.display()
                ),
            ));
        }
        let dir = path
            .parent()
            .ok_or_else(|| invalid("store path has no parent"))?;
        std::fs::create_dir_all(dir)
            .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
        let conn = Connection::open(path).map_err(sql_err)?;
        schema::create_sage_schema(&conn).map_err(sql_err)?;
        schema::apply_extension(&conn, now_iso).map_err(sql_err)?;
        drop(conn);
        let manifest = dir.join("manifest.json");
        if !manifest.exists() {
            let body = serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1, "createdAt": now_iso, "updatedAt": now_iso
            }))
            .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
            std::fs::write(manifest, body)
                .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn writable(&self) -> bool {
        self.writable
    }
    pub fn fts(&self) -> bool {
        self.fts
    }

    /// Re-run the read-only inspection on the live connection.
    pub fn inspect(&self) -> StoreResult<Inspection> {
        inspect::inspect_conn(&self.path, &self.conn).map_err(sql_err)
    }

    // ─── Reads ──────────────────────────────────────────────────────────────

    /// Exact-ID lookup with the caller's visibility. `None` for hidden,
    /// absent, or undecodable rows (indistinguishable to the caller).
    pub fn get(&self, id: &str, viewer: &Viewer) -> StoreResult<Option<SageRecord>> {
        let row: Option<String> = self
            .conn
            .query_row(
                &format!(
                    "SELECT data FROM memories WHERE id = ?1 AND {}",
                    session_clause("", viewer, 2)
                ),
                params![id, viewer.session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        // Baseline `getSage`: an undecodable row reads as absent (corruption
        // is reported by search counts and doctor, not by exact-ID shape).
        Ok(row.and_then(|data| decode_memory(&data)))
    }

    /// Lexical search, ported from `searchSage` (`sqlite-store-search-sage.ts`):
    ///
    /// * terms from [`fts_prefix_terms`]; FTS MATCH with implicit AND, retried
    ///   with OR when AND finds nothing and there is more than one term;
    /// * `ORDER BY bm25 ASC, importance DESC`;
    /// * no usable term (or no FTS5) -> `unicode_text LIKE %nfkc_lower(query)%`
    ///   ordered by importance; empty query -> importance, recency;
    /// * statuses default to `active`; when the caller does not pass statuses
    ///   (the automatic-context shape) `contextPolicy = never` is excluded.
    ///
    /// Not ported: `hybridRerankMemories` for queries of two or more tokens.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        kinds: Option<&[String]>,
        statuses: Option<&[String]>,
        viewer: &Viewer,
    ) -> StoreResult<SearchResult> {
        let limit = limit.clamp(1, MAX_PAGE_SIZE);
        let explicit_statuses = statuses.is_some_and(|s| !s.is_empty());
        let default_status = ["active".to_string()];
        let statuses = statuses
            .filter(|s| !s.is_empty())
            .unwrap_or(&default_status);
        for s in statuses {
            if !SAGE_STATUSES.contains(&s.as_str()) {
                return Err(invalid(format!("unknown status {s}")));
            }
        }
        if let Some(k) = kinds {
            for kind in k {
                if !SAGE_KINDS.contains(&kind.as_str()) {
                    return Err(invalid(format!("unknown kind {kind}")));
                }
            }
        }
        let filter = Filter {
            statuses,
            kinds,
            exclude_never: !explicit_statuses,
            viewer,
        };
        let query = query.trim();
        if query.is_empty() {
            return self.select_hits(
                &filter,
                "",
                vec![],
                "ORDER BY m.importance DESC, m.updated_at DESC",
                limit,
                "recent",
            );
        }
        let terms = fts_prefix_terms(query);
        if self.fts && !terms.is_empty() {
            let first = self.select_fts(&filter, &terms.join(" "), limit)?;
            if terms.len() < 2 {
                return Ok(first);
            }
            let mut result = if first.hits.is_empty() {
                self.select_fts(&filter, &terms.join(" OR "), limit)?
            } else {
                first
            };
            crate::retrieval::hybrid_rerank(query, &mut result.hits);
            return Ok(result);
        }
        self.search_like(&filter, query, limit)
    }

    fn select_fts(&self, f: &Filter<'_>, expr: &str, limit: usize) -> StoreResult<SearchResult> {
        self.select_hits(
            f,
            "memories_fts MATCH ?",
            vec![json!(expr)],
            "ORDER BY bm25(memories_fts) ASC, m.importance DESC",
            limit,
            "fts",
        )
    }

    fn search_like(&self, f: &Filter<'_>, query: &str, limit: usize) -> StoreResult<SearchResult> {
        let needle = unicode_fold(query);
        if self.unicode_text && self.writable {
            self.refresh_unicode_text()?;
            let pattern = format!("%{}%", escape_like(&needle));
            return self.select_hits(
                f,
                "m.unicode_text LIKE ? ESCAPE '\\'",
                vec![json!(pattern)],
                "ORDER BY m.importance DESC",
                limit,
                "like",
            );
        }
        // Read-only handle: never write the lazily-refreshed projection; fold
        // in process over a bounded, visibility-filtered scan instead.
        let (where_sql, args) = f.sql("", vec![]);
        let text_col = if self.unicode_text {
            "m.unicode_text"
        } else {
            "NULL"
        };
        let sql = format!(
            "SELECT m.data, {text_col} FROM memories m WHERE {where_sql}
              ORDER BY m.importance DESC LIMIT {FALLBACK_SCAN_LIMIT}"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(params_from_iter(args.iter().map(sql_value)), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map_err(sql_err)?;
        let mut hits = Vec::new();
        let mut corrupt = 0;
        let mut scanned = 0usize;
        for row in rows {
            scanned += 1;
            let (data, folded) = row.map_err(sql_err)?;
            let Some(memory) = decode_memory(&data) else {
                corrupt += 1;
                continue;
            };
            let folded = folded.unwrap_or_else(|| unicode_fold(&memory.text));
            if folded.contains(&needle) {
                hits.push(SearchHit {
                    memory,
                    channel: "like".into(),
                    score: 1.0,
                    reasons: vec![],
                });
                if hits.len() > limit {
                    break;
                }
            }
        }
        let truncated = hits.len() > limit || scanned >= FALLBACK_SCAN_LIMIT;
        hits.truncate(limit);
        Ok(SearchResult {
            hits,
            truncated,
            corrupt_skipped: corrupt,
            channel: "like".into(),
        })
    }

    /// `withSageTextIndex`: record the projection revision and fill NULL
    /// `unicode_text` values (writable handles only).
    fn refresh_unicode_text(&self) -> StoreResult<()> {
        let conn = &self.conn;
        conn.execute_batch("SAVEPOINT ws_text_index")
            .map_err(sql_err)?;
        let result = (|| -> rusqlite::Result<()> {
            let current: Option<String> = conn
                .query_row(
                    "SELECT revision FROM ws_text_index_revisions WHERE name = 'memories.unicode_text'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if current.as_deref() != Some(crate::schema::TEXT_INDEX_REVISION) {
                conn.execute("UPDATE memories SET unicode_text = NULL", [])?;
                conn.execute(
                    "INSERT INTO ws_text_index_revisions(name, revision) VALUES ('memories.unicode_text', ?1)
                     ON CONFLICT(name) DO UPDATE SET revision = excluded.revision",
                    [crate::schema::TEXT_INDEX_REVISION],
                )?;
            }
            let pending: Vec<(String, Option<String>)> = conn
                .prepare(
                    "SELECT id, CASE WHEN json_valid(data) AND json_type(data, '$.text') = 'text'
                                     THEN json_extract(data, '$.text') END
                       FROM memories WHERE unicode_text IS NULL",
                )?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut update = conn.prepare("UPDATE memories SET unicode_text = ?1 WHERE id = ?2")?;
            for (id, text) in pending {
                update.execute(params![unicode_fold(text.as_deref().unwrap_or("")), id])?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("RELEASE ws_text_index").map_err(sql_err),
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK TO ws_text_index; RELEASE ws_text_index");
                Err(sql_err(e))
            }
        }
    }

    pub(crate) fn select_hits(
        &self,
        f: &Filter<'_>,
        extra_where: &str,
        extra_args: Vec<Value>,
        order: &str,
        limit: usize,
        channel: &str,
    ) -> StoreResult<SearchResult> {
        let from = if channel == "fts" {
            "memories_fts CROSS JOIN memories m ON m.rowid = memories_fts.rowid"
        } else {
            "memories m"
        };
        let (where_sql, args) = f.sql(extra_where, extra_args);
        let sql = format!(
            "SELECT m.data FROM {from} WHERE {where_sql} {order} LIMIT {}",
            limit + 1
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(params_from_iter(args.iter().map(sql_value)), |r| {
                r.get::<_, String>(0)
            })
            .map_err(sql_err)?;
        let mut hits = Vec::new();
        let mut corrupt = 0;
        for (rank, row) in rows.enumerate() {
            let data = row.map_err(sql_err)?;
            match decode_memory(&data) {
                Some(memory) => hits.push(SearchHit {
                    memory,
                    channel: channel.to_string(),
                    // Rank-derived, higher is better; not comparable across channels.
                    score: 1.0 / (rank as f64 + 1.0),
                    reasons: vec![],
                }),
                None => corrupt += 1,
            }
        }
        let truncated = hits.len() + corrupt > limit;
        hits.truncate(limit);
        Ok(SearchResult {
            hits,
            truncated,
            corrupt_skipped: corrupt,
            channel: channel.to_string(),
        })
    }

    /// Memories anchored at `path`, at an ancestor directory of it, or (for a
    /// directory query) below it. Active/stale only, visibility-bound.
    pub fn for_path(&self, path: &str, limit: usize, viewer: &Viewer) -> StoreResult<SearchResult> {
        if path.trim().is_empty() {
            return Err(invalid("path must not be empty"));
        }
        self.retrieve_for_paths(
            &[path.to_string()],
            &crate::retrieval::PathOptions {
                limit: Some(limit),
                ..Default::default()
            },
            viewer,
        )
    }

    /// Review queue. Session-scoped proposals are visible to their session only.
    pub fn list_candidates(
        &self,
        status: &str,
        limit: usize,
        viewer: &Viewer,
    ) -> StoreResult<(Vec<MemoryCandidate>, bool, usize)> {
        if !CANDIDATE_STATUSES.contains(&status) {
            return Err(invalid(format!("unknown candidate status {status}")));
        }
        let limit = limit.clamp(1, MAX_PAGE_SIZE);
        let mut stmt = self
            .conn
            .prepare(
                // Visibility before LIMIT: hidden session proposals must not
                // consume the page budget and starve visible ones (WM-603).
                // Undecodable rows stay in the scan so they are counted.
                "SELECT data FROM candidates WHERE status = ?1
                   AND (NOT json_valid(data)
                        OR json_extract(data, '$.scope') IS NOT 'session'
                        OR (json_extract(data, '$.ownerSessionId') IS NOT NULL
                            AND json_extract(data, '$.ownerSessionId') = ?3))
                  ORDER BY created_at DESC, id DESC LIMIT ?2",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(
                params![status, (limit * 4 + 1) as i64, viewer.session_id],
                |r| r.get::<_, String>(0),
            )
            .map_err(sql_err)?;
        let mut out = Vec::new();
        let mut corrupt = 0;
        for row in rows {
            let data = row.map_err(sql_err)?;
            match serde_json::from_str::<MemoryCandidate>(&data) {
                Ok(c) => {
                    // Fail closed: an unowned session proposal is visible to no one.
                    let visible = c.scope != "session"
                        || (c.owner_session_id.is_some()
                            && c.owner_session_id == viewer.session_id);
                    if visible {
                        out.push(c);
                    }
                }
                Err(_) => corrupt += 1,
            }
            if out.len() > limit {
                break;
            }
        }
        let truncated = out.len() > limit;
        out.truncate(limit);
        Ok((out, truncated, corrupt))
    }

    // ─── Mutations ──────────────────────────────────────────────────────────

    /// Create an authoritative memory. Runs in one IMMEDIATE transaction with
    /// the receipt, so a retry after "commit before reply" returns the
    /// original outcome (WM-INV-05/06).
    pub fn remember(
        &mut self,
        p: &RememberParams,
        ctx: &MutationCtx,
    ) -> StoreResult<MutationOutcome> {
        self.remember_full(p, ctx)
    }

    /// Create a pending review candidate. Never touches `memories`.
    pub fn propose(
        &mut self,
        p: &ProposeParams,
        ctx: &MutationCtx,
    ) -> StoreResult<MutationOutcome> {
        self.require_writable()?;
        let fingerprint = fingerprint(&serde_json::to_value(p).unwrap_or(Value::Null));
        let candidate = self.build_candidate(p, ctx)?;
        self.with_receipt(ctx, "candidate.propose", &fingerprint, |conn| (|| -> rusqlite::Result<MutationOutcome> {
            // Baseline dedupe: an identical pending proposal is returned as is.
            let canonical = normalize_text_key(&candidate.text);
            let pending: Vec<String> = conn
                .prepare("SELECT data FROM candidates WHERE status = 'pending' AND canonical_text = ?1")?
                .query_map([&canonical], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let vkey = |c: &MemoryCandidate| serde_json::to_string(&c.validity).unwrap_or_default();
            for d in pending {
                if let Ok(existing) = serde_json::from_str::<MemoryCandidate>(&d)
                    && existing.scope == candidate.scope
                    && existing.owner_session_id == candidate.owner_session_id
                    && existing.target_memory_id == candidate.target_memory_id
                    && existing.target_revision == candidate.target_revision
                    && vkey(&existing) == vkey(&candidate)
                {
                    return Ok(MutationOutcome {
                        outcome: "candidate-pending".into(),
                        id: existing.id.clone(),
                        revision: None,
                        replayed: false,
                        record: serde_json::to_value(&existing).unwrap_or(Value::Null),
                    });
                }
            }
            let data = serde_json::to_string(&candidate)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            conn.execute(
                "INSERT INTO candidates (id, data, status, created_at, updated_at, canonical_text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    candidate.id,
                    data,
                    candidate.status,
                    candidate.created_at,
                    candidate.updated_at,
                    normalize_text_key(&candidate.text)
                ],
            )?;
            audit(
                conn,
                "memory.candidate_created",
                &ctx.now_iso,
                json!({"details": {"candidateId": candidate.id}}),
            )?;
            Ok(MutationOutcome {
                outcome: "candidate-pending".into(),
                id: candidate.id.clone(),
                revision: None,
                replayed: false,
                record: serde_json::to_value(&candidate).unwrap_or(Value::Null),
            })
        })().map_err(sql_err))
    }

    pub(crate) fn require_writable(&self) -> StoreResult<()> {
        if self.writable {
            Ok(())
        } else {
            Err(ErrorBody::new(
                ErrorCode::ReadOnly,
                "store is admitted read-only (no service ownership)",
            ))
        }
    }

    pub(crate) fn with_receipt(
        &mut self,
        ctx: &MutationCtx,
        action: &str,
        fingerprint: &str,
        work: impl FnOnce(&Connection) -> StoreResult<MutationOutcome>,
    ) -> StoreResult<MutationOutcome> {
        let conn = &self.conn;
        // Nested use (candidate accept/resolve composing remember/update)
        // runs inside the caller's transaction as a savepoint.
        let nested = !conn.is_autocommit();
        conn.execute_batch(if nested {
            "SAVEPOINT wm_nested"
        } else {
            "BEGIN IMMEDIATE"
        })
        .map_err(sql_err)?;
        let result: Result<MutationOutcome, ErrorBody> = (|| {
            if let Some(key) = &ctx.idempotency_key {
                let prior: Option<(String, String)> = conn
                    .query_row(
                        "SELECT fingerprint, result FROM wm_mutation_receipts
                          WHERE principal_id = ?1 AND action = ?2 AND idempotency_key = ?3",
                        params![ctx.principal_id, action, key],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(sql_err)?;
                if let Some((fp, stored)) = prior {
                    if fp != fingerprint {
                        return Err(ErrorBody::new(
                            ErrorCode::IdempotencyMismatch,
                            "idempotency key was already used with a different payload",
                        ));
                    }
                    let mut outcome: MutationOutcome = serde_json::from_str(&stored)
                        .map_err(|e| ErrorBody::new(ErrorCode::DatabaseCorrupt, e.to_string()))?;
                    outcome.replayed = true;
                    return Ok(outcome);
                }
            }
            let outcome = work(conn)?;
            if let Some(key) = &ctx.idempotency_key {
                let stored = serde_json::to_string(&outcome)
                    .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
                conn.execute(
                    "INSERT INTO wm_mutation_receipts
                       (principal_id, action, idempotency_key, fingerprint, result, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        ctx.principal_id,
                        action,
                        key,
                        fingerprint,
                        stored,
                        ctx.now_iso
                    ],
                )
                .map_err(sql_err)?;
            }
            Ok(outcome)
        })();
        // Fault injection (debug builds only, see wrongmemoryd/src/fault.rs):
        // fail the outermost transaction before COMMIT.
        let result = match result {
            Ok(_) if !nested && injected_before_commit() => Err(ErrorBody::new(
                ErrorCode::Internal,
                "injected fault before_commit (disk I/O error)",
            )),
            other => other,
        };
        match result {
            Ok(v) => {
                conn.execute_batch(if nested {
                    "RELEASE wm_nested"
                } else {
                    "COMMIT"
                })
                .map_err(sql_err)?;
                Ok(v)
            }
            Err(e) => {
                if nested {
                    let _ = conn.execute_batch("ROLLBACK TO wm_nested; RELEASE wm_nested");
                } else if !conn.is_autocommit() {
                    let _ = conn.execute_batch("ROLLBACK");
                }
                Err(e)
            }
        }
    }

    /// Run `work` in one IMMEDIATE transaction (outermost composite ops).
    pub(crate) fn in_transaction<T>(
        &mut self,
        work: impl FnOnce(&mut Self) -> StoreResult<T>,
    ) -> StoreResult<T> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(sql_err)?;
        match work(self) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT").map_err(sql_err)?;
                Ok(v)
            }
            Err(e) => {
                if !self.conn.is_autocommit() {
                    let _ = self.conn.execute_batch("ROLLBACK");
                }
                Err(e)
            }
        }
    }

    pub(crate) fn relative_path(&self, p: &str) -> String {
        let normalized = normalize_slashes(p.trim());
        if let Some(root) = &self.project_root {
            let root_s = normalize_slashes(&root.to_string_lossy());
            let root_s = root_s.trim_end_matches('/');
            let lower = normalized.to_lowercase();
            let root_lower = root_s.to_lowercase();
            if cfg!(windows) && lower.starts_with(&format!("{root_lower}/")) {
                return normalized[root_s.len() + 1..].to_string();
            }
            if let Some(rest) = normalized.strip_prefix(&format!("{root_s}/")) {
                return rest.to_string();
            }
        }
        normalized
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string()
    }

    fn validate_common(&self, c: &CommonInput<'_>) -> StoreResult<()> {
        let CommonInput {
            text,
            kind,
            scope,
            tags,
            anchors,
            sources_len,
            validity,
        } = *c;
        if normalize_text(text).is_empty() {
            return Err(invalid("text must not be empty"));
        }
        if js_length(text) > MAX_TEXT_CHARS {
            return Err(invalid(format!("text exceeds {MAX_TEXT_CHARS} characters")));
        }
        if !SAGE_KINDS.contains(&kind) {
            return Err(invalid(format!("unknown kind {kind}")));
        }
        if !SAGE_SCOPES.contains(&scope) {
            return Err(invalid(format!("unknown scope {scope}")));
        }
        if tags.len() > MAX_METADATA_ITEMS
            || anchors.len() > MAX_METADATA_ITEMS
            || sources_len > MAX_METADATA_ITEMS
        {
            return Err(invalid(format!(
                "at most {MAX_METADATA_ITEMS} metadata items"
            )));
        }
        for a in anchors {
            if !ANCHOR_TYPES.contains(&a.kind.as_str()) {
                return Err(invalid(format!("unknown anchor type {}", a.kind)));
            }
        }
        if let Some(v) = validity {
            if v.statement.trim().is_empty() {
                return Err(invalid("validity.statement must not be empty"));
            }
            if let Some(checks) = &v.checks {
                if checks.len() > MAX_VALIDITY_CHECKS {
                    return Err(invalid(format!(
                        "at most {MAX_VALIDITY_CHECKS} validity checks"
                    )));
                }
                if checks.iter().any(|c| c.kind != "source_contains") {
                    return Err(invalid("only literal source_contains checks are supported"));
                }
            }
        }
        Ok(())
    }

    fn normalize_anchors(&self, anchors: &[MemoryAnchor]) -> Vec<MemoryAnchor> {
        anchors
            .iter()
            .map(|a| {
                let mut a = a.clone();
                if let Some(p) = &a.path {
                    a.path = Some(self.relative_path(p));
                }
                a
            })
            .collect()
    }

    fn build_candidate(
        &self,
        p: &ProposeParams,
        ctx: &MutationCtx,
    ) -> StoreResult<MemoryCandidate> {
        crate::rules::reject_if_unsafe(&serde_json::to_value(p).unwrap_or(Value::Null))?;
        let default_sources = [wrongmemory_contracts::MemorySource {
            kind: "session".into(),
            ..Default::default()
        }];
        let kind = p.kind.clone().unwrap_or_else(|| "fact".into());
        let scope = p.scope.clone().unwrap_or_else(|| "project".into());
        self.validate_common(&CommonInput {
            text: &p.text,
            kind: &kind,
            scope: &scope,
            tags: &p.tags,
            anchors: &p.anchors,
            sources_len: p.sources.len(),
            validity: p.validity.as_ref(),
        })?;
        if let Some(a) = &p.suggested_action
            && !SUGGESTED_ACTIONS.contains(&a.as_str())
        {
            return Err(invalid(format!("unknown suggested action {a}")));
        }
        if let Some(rev) = p.target_revision
            && (p.target_memory_id.is_none() || rev < 1)
        {
            return Err(invalid(
                "SAGE targetRevision requires a targetMemoryId and a positive integer revision.",
            ));
        }
        validate_supersedes(p)?;
        // A proposal may only name memories the proposer can see, so it can
        // neither probe hidden IDs nor have a curator retire another
        // session's private row (WM-INV-08). Hidden and absent read alike.
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        for member in p.supersedes_memory_ids.iter().flatten() {
            if self.get(member, &viewer)?.is_none() {
                return Err(ErrorBody::new(
                    ErrorCode::NotFound,
                    "supersedes member not found",
                ));
            }
        }
        if let Some(target) = &p.target_memory_id {
            // A stale `targetRevision` is accepted here, as in the baseline:
            // resolution passes it as `expectedRevision`, so a stale review
            // fails there and the candidate stays pending.
            if self.get(target, &viewer)?.is_none() {
                return Err(ErrorBody::new(
                    ErrorCode::NotFound,
                    "target memory not found",
                ));
            }
        }
        let owner_session_id = if scope == "session" {
            Some(ctx.session_id.clone().ok_or_else(|| {
                invalid("scope=session requires a bound session (session.acquire)")
            })?)
        } else {
            None
        };
        let mut extra = Map::new();
        extra.insert(
            "proposedBy".into(),
            json!({"principalId": ctx.principal_id, "client": ctx.client_label, "service": "wrongmemory"}),
        );
        Ok(MemoryCandidate {
            schema_version: 1,
            id: format!("candidate_{}", ulid::Ulid::generate()),
            status: "pending".into(),
            text: normalize_text(&p.text),
            kind,
            scope,
            owner_session_id,
            confidence: clamp01(p.confidence.unwrap_or(0.6)),
            importance: clamp01(p.importance.unwrap_or(0.6)),
            tags: normalize_tags(&p.tags),
            anchors: self.normalize_anchors(&p.anchors),
            audience: p.audience.clone(),
            sources: crate::mutate::normalize_sources(if p.sources.is_empty() {
                &default_sources
            } else {
                &p.sources
            }),
            validity: p.validity.clone(),
            created_at: ctx.now_iso.clone(),
            updated_at: ctx.now_iso.clone(),
            memory_id: None,
            reason: None,
            target_memory_id: p.target_memory_id.clone(),
            target_revision: p.target_revision,
            review_reason: p.review_reason.clone(),
            suggested_action: p.suggested_action.clone(),
            supersedes_memory_ids: p.supersedes_memory_ids.clone(),
            supersedes_revisions: p.supersedes_revisions.clone(),
            extra,
        })
    }
}

/// `validateSupersedes`: a consolidation proposal names 1-50 distinct member
/// ids with one positive revision each. Members are not looked up here; one
/// the curator cannot see or that changed is skipped at accept.
fn validate_supersedes(p: &ProposeParams) -> StoreResult<()> {
    const MAX_SUPERSEDED_MEMBERS: usize = 50;
    let (ids, revs) = match (&p.supersedes_memory_ids, &p.supersedes_revisions) {
        (None, None) => return Ok(()),
        (Some(ids), Some(revs)) => (ids, revs),
        _ => return Err(bad_supersedes()),
    };
    let distinct: std::collections::HashSet<&String> = ids.iter().collect();
    if ids.is_empty()
        || ids.len() != revs.len()
        || ids.len() > MAX_SUPERSEDED_MEMBERS
        || distinct.len() != ids.len()
        || ids
            .iter()
            .any(|id| id.is_empty() || crate::text::js_length(id) > 200)
        || revs.iter().any(|r| *r < 1)
    {
        return Err(bad_supersedes());
    }
    Ok(())
}

fn bad_supersedes() -> ErrorBody {
    invalid(
        "SAGE supersedesMemoryIds needs 1-50 distinct ids with one positive integer revision each.",
    )
}

fn clamp01(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Input fields shared by `remember` and `propose` validation.
#[derive(Clone, Copy)]
struct CommonInput<'a> {
    text: &'a str,
    kind: &'a str,
    scope: &'a str,
    tags: &'a [String],
    anchors: &'a [MemoryAnchor],
    sources_len: usize,
    validity: Option<&'a wrongmemory_contracts::MemoryValidity>,
}

/// `tokenize` (memory-text.ts); see [`crate::text::tokenize_default`].
pub fn tokenize(text: &str) -> Vec<String> {
    crate::text::tokenize_default(text)
}

/// `normalizeTags`: strip one leading `#`, trim, lowercase, drop empty, dedupe.
fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.strip_prefix('#').unwrap_or(t).trim().to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Decode a stored memory; `None` marks a corrupt row (reported, not fatal).
pub fn decode_memory(data: &str) -> Option<SageRecord> {
    serde_json::from_str(data).ok()
}

/// `sageToLegacyScope`.
pub(crate) fn legacy_scope_for(scope: &str) -> &'static str {
    if scope == "user" {
        "user-memory"
    } else {
        "project-memory"
    }
}

pub(crate) fn insert_memory(conn: &Connection, m: &SageRecord) -> rusqlite::Result<()> {
    let data = serde_json::to_string(m)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let audience = m
        .audience
        .as_ref()
        .map(|a| serde_json::to_string(a).unwrap_or_default());
    let tags = serde_json::to_string(&m.tags).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO memories
           (id, data, status, kind, scope, legacy_scope, importance, confidence, freshness,
            updated_at, created_at, audience, tags, owner_session_id, canonical_text, unicode_text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            m.id,
            data,
            m.status,
            m.kind,
            m.scope,
            m.legacy_scope
                .as_deref()
                .unwrap_or(legacy_scope_for(&m.scope)),
            m.importance,
            m.confidence,
            m.freshness,
            m.updated_at,
            m.created_at,
            audience,
            tags,
            m.owner_session_id,
            normalize_text_key(&m.text),
            unicode_fold(&m.text),
        ],
    )?;
    Ok(())
}

/// Port of `sqliteAnchorNode`.
pub fn anchor_node(a: &MemoryAnchor) -> Option<String> {
    let path = a
        .path
        .as_deref()
        .map(normalize_slashes)
        .filter(|p| !p.is_empty());
    match a.kind.as_str() {
        "file" | "test" | "git" => path.map(|p| format!("file:{p}")),
        "directory" | "package" => path.map(|p| format!("dir:{p}")),
        "symbol" => match (path, a.symbol.as_deref()) {
            (Some(p), Some(s)) if !s.is_empty() => Some(format!("symbol:{p}#{s}")),
            _ => None,
        },
        "command" => a
            .command
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(|c| format!("command:{}", crate::text::collapse_js_space(c))),
        "agent" => a
            .role
            .as_deref()
            .filter(|r| !r.is_empty())
            .map(|r| format!("agent:{}", r.trim().to_lowercase())),
        _ => None,
    }
}

/// Port of `sqliteAnchorRelation`.
pub fn anchor_relation(a: &MemoryAnchor) -> Option<&'static str> {
    Some(match a.kind.as_str() {
        "file" | "test" | "git" => "about_file",
        "directory" => "about_directory",
        "symbol" => "about_symbol",
        "package" => "about_package",
        "command" => "about_command",
        "agent" => "about_agent",
        _ => return None,
    })
}

/// Port of `syncSqliteAnchorEdges` (weight policy: MAX on conflict).
pub(crate) fn sync_anchor_edges(
    conn: &Connection,
    m: &SageRecord,
    now: &str,
) -> rusqlite::Result<()> {
    let from = format!("mem:{}", m.id);
    let mut created: std::collections::HashMap<(String, String), String> = Default::default();
    {
        let mut del = conn.prepare(
            "DELETE FROM edges WHERE from_node = ?1 AND relation GLOB 'about_*'
             RETURNING to_node, relation, created_at",
        )?;
        let rows = del.query_map([&from], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (k, v) = row?;
            created.insert(k, v);
        }
    }
    if m.status != "active" && m.status != "stale" {
        return Ok(());
    }
    let mut insert = conn.prepare(
        "INSERT INTO edges (from_node, to_node, relation, weight, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(from_node, to_node, relation) DO UPDATE SET
           weight = MAX(weight, excluded.weight)",
    )?;
    for a in &m.anchors {
        if let (Some(target), Some(rel)) = (anchor_node(a), anchor_relation(a)) {
            let at = created
                .get(&(target.clone(), rel.to_string()))
                .cloned()
                .unwrap_or_else(|| now.to_string());
            insert.execute(params![from, target, rel, m.confidence, at])?;
        }
        let Some(p) = a.path.as_deref() else { continue };
        let anchored = normalize_slashes(p);
        let is_dir = a.kind == "directory" || a.kind == "package";
        let file_node = format!("file:{anchored}");
        let dir_path = if is_dir {
            anchored.clone()
        } else {
            normalize_slashes(&posix_dirname(&anchored))
        };
        if a.kind == "symbol"
            && let Some(sym) = a.symbol.as_deref().filter(|s| !s.is_empty())
        {
            insert.execute(params![
                format!("symbol:{anchored}#{sym}"),
                file_node,
                "related_to",
                m.confidence,
                now
            ])?;
        }
        if !is_dir {
            insert.execute(params![
                file_node,
                format!("dir:{dir_path}"),
                "related_to",
                m.confidence,
                now
            ])?;
        }
        let dirs = ancestor_paths(&dir_path);
        for w in dirs.windows(2) {
            insert.execute(params![
                format!("dir:{}", w[0]),
                format!("dir:{}", w[1]),
                "related_to",
                m.confidence,
                now
            ])?;
        }
    }
    Ok(())
}

pub(crate) fn audit(conn: &Connection, event: &str, at: &str, data: Value) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO audit_log (event, at, trace_id, data) VALUES (?1, ?2, NULL, ?3)",
        params![event, at, data.to_string()],
    )?;
    let id = conn.last_insert_rowid();
    if id % AUDIT_LOG_PRUNE_INTERVAL == 0 {
        conn.execute(
            "DELETE FROM audit_log WHERE id <= ?1",
            params![id - AUDIT_LOG_MAX_ROWS],
        )?;
    }
    Ok(())
}

fn push_in(sql: &mut String, args: &mut Vec<Value>, column: &str, values: &[String]) {
    sql.push_str(&format!(" AND {column} IN ("));
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        args.push(json!(v));
        sql.push_str(&format!("?{}", args.len()));
    }
    sql.push(')');
}

fn sql_value(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        Value::Number(n) => n
            .as_i64()
            .map(rusqlite::types::Value::Integer)
            .unwrap_or_else(|| rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0))),
        other => rusqlite::types::Value::Text(other.to_string()),
    }
}

/// `ftsPrefixTerms`: split on JS `\s+`, then on `[^\p{L}\p{N}_]+`; drop
/// tokens shorter than 2 UTF-16 units; emit `"token"*` (quotes removed);
/// at most 64 terms. The query is not case/NFKC-folded here: FTS5's
/// `unicode61` tokenizer folds case and diacritics, but not NFKC (so `file`
/// does not match `ﬁle` through FTS — baseline behavior).
///
/// `\p{L}` / `\p{N}` are approximated by `char::is_alphabetic` /
/// `char::is_numeric`; `is_alphabetic` also admits Other_Alphabetic marks.
pub fn fts_prefix_terms(query: &str) -> Vec<String> {
    const MAX_FTS_PREFIX_TERMS: usize = 64;
    let mut terms = Vec::new();
    for word in query.split(crate::text::is_js_space) {
        if word.is_empty() {
            continue;
        }
        for token in word.split(|c: char| !(c.is_alphabetic() || c.is_numeric() || c == '_')) {
            if token.is_empty() || js_length(token) < 2 {
                continue;
            }
            terms.push(format!("\"{}\"*", token.replace('"', "")));
            if terms.len() >= MAX_FTS_PREFIX_TERMS {
                return terms;
            }
        }
    }
    terms
}

/// SAGE `buildSessionClause` without the `includeAllSessions` bypass: with a
/// bound session, session rows owned by it; without one, only legacy unowned
/// session rows (owner NULL). `n` is the positional index of the session arg.
pub(crate) fn session_clause(prefix: &str, viewer: &Viewer, n: usize) -> String {
    if viewer.session_id.is_some() {
        format!("({prefix}scope != 'session' OR {prefix}owner_session_id = ?{n})")
    } else {
        // `?n IS NULL` keeps the positional parameter referenced (and true).
        format!(
            "({prefix}scope != 'session' OR ({prefix}owner_session_id IS NULL AND ?{n} IS NULL))"
        )
    }
}

pub(crate) struct Filter<'a> {
    pub(crate) statuses: &'a [String],
    pub(crate) kinds: Option<&'a [String]>,
    pub(crate) exclude_never: bool,
    pub(crate) viewer: &'a Viewer,
}

impl Filter<'_> {
    /// WHERE clause (visibility first) plus positional args. `extra` may
    /// contain one `?` placeholder per `extra_args` element, in order.
    fn sql(&self, extra: &str, extra_args: Vec<Value>) -> (String, Vec<Value>) {
        let mut args: Vec<Value> = vec![json!(self.viewer.session_id)];
        let mut sql = session_clause("m.", self.viewer, 1);
        push_in(&mut sql, &mut args, "m.status", self.statuses);
        if let Some(k) = self.kinds {
            push_in(&mut sql, &mut args, "m.kind", k);
        }
        if self.exclude_never {
            sql.push_str(
                " AND (CASE WHEN json_valid(m.data) THEN json_extract(m.data, '$.contextPolicy') END) IS NOT 'never'",
            );
        }
        if !extra.is_empty() {
            let mut rendered = String::new();
            let mut it = extra_args.into_iter();
            for ch in extra.chars() {
                if ch == '?' {
                    args.push(it.next().unwrap_or(Value::Null));
                    rendered.push_str(&format!("?{}", args.len()));
                } else {
                    rendered.push(ch);
                }
            }
            sql.push_str(" AND ");
            sql.push_str(&rendered);
        }
        (sql, args)
    }
}

fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Canonical payload fingerprint: SHA-256 of key-sorted compact JSON.
pub fn fingerprint(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    out.insert(k.clone(), sorted(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    hex::encode(Sha256::digest(sorted(v).to_string().as_bytes()))
}

/// `WRONGMEMORY_FAULT=before_commit` (debug builds only): fails the first
/// outermost receipt transaction of the process, once, so tests can prove
/// the rollback left the connection usable for the next write.
fn injected_before_commit() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static FIRED: AtomicBool = AtomicBool::new(false);
    cfg!(debug_assertions)
        && std::env::var("WRONGMEMORY_FAULT")
            .map(|v| v.split(':').next() == Some("before_commit"))
            .unwrap_or(false)
        && !FIRED.swap(true, Ordering::SeqCst)
}

/// `WRONGMEMORY_FAULT=<point>[:...]` names `point` (debug builds only).
fn fault_is(point: &str) -> bool {
    cfg!(debug_assertions)
        && std::env::var("WRONGMEMORY_FAULT")
            .map(|v| v.split(':').next() == Some(point))
            .unwrap_or(false)
}
