//! SAGE SQLite DDL (`sqlite-store-schema.ts`, `sqlite-store-initialize.ts`,
//! `persistence/src/sqlite-text-index.ts` at the baseline commit) and the
//! additive WrongMemory extension.
//!
//! The DDL is replayed in the same order SAGE applies it so a managed store is
//! indistinguishable from one SAGE created (verified against the oracle's
//! `contracts/profiles/sage-sqlite-v5-baseline/schema.sql`).

use rusqlite::Connection;

/// Connection pragmas SAGE sets on every open (cache/mmap sizes are
/// host-tuned there and are not part of the storage contract).
pub const CONNECTION_PRAGMAS: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 30000;
PRAGMA temp_store = MEMORY;
PRAGMA foreign_keys = ON;
";

/// Tables a v5 SAGE store contains (FTS shadow tables excluded).
pub const SAGE_TABLES: &[&str] = &[
    "schema_meta",
    "memories",
    "ws_text_index_revisions",
    "memories_fts",
    "edges",
    "audit_log",
    "candidates",
    "hq_memory_sync",
    "hq_memory_sync_clock",
];

/// Required `memories` columns for v5 read admission. `unicode_text` is
/// optional: older writers lack it and readers fold on the fly.
pub const MEMORY_COLUMNS: &[&str] = &[
    "id",
    "data",
    "status",
    "kind",
    "scope",
    "legacy_scope",
    "importance",
    "confidence",
    "freshness",
    "updated_at",
    "created_at",
    "audience",
    "tags",
    "owner_session_id",
    "canonical_text",
];

pub const CANDIDATE_COLUMNS: &[&str] = &[
    "id",
    "data",
    "status",
    "created_at",
    "updated_at",
    "canonical_text",
];

pub const EDGE_COLUMNS: &[&str] = &["from_node", "to_node", "relation", "weight", "created_at"];

/// Core DDL in SAGE's order (initSchema -> initRemainingSchema -> post-migration indexes).
const CORE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_meta (
  key TEXT PRIMARY KEY,
  value INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS memories (
  id TEXT PRIMARY KEY,
  data TEXT NOT NULL,
  status TEXT NOT NULL,
  kind TEXT NOT NULL,
  scope TEXT NOT NULL,
  legacy_scope TEXT,
  importance REAL NOT NULL,
  confidence REAL NOT NULL,
  freshness REAL NOT NULL,
  updated_at TEXT NOT NULL,
  created_at TEXT NOT NULL,
  audience TEXT,
  tags TEXT,
  owner_session_id TEXT,
  canonical_text TEXT NOT NULL DEFAULT ''
);
ALTER TABLE memories ADD COLUMN unicode_text TEXT;
CREATE TABLE IF NOT EXISTS ws_text_index_revisions (name TEXT PRIMARY KEY, revision TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_memories_unicode_text_pending
  ON memories(id) WHERE unicode_text IS NULL;
CREATE TRIGGER IF NOT EXISTS memories_unicode_text_invalidate
  AFTER UPDATE OF data ON memories
  WHEN (CASE WHEN json_valid(OLD.data) THEN json_extract(OLD.data, '$.text') END)
    IS NOT (CASE WHEN json_valid(NEW.data) THEN json_extract(NEW.data, '$.text') END)
  BEGIN UPDATE memories SET unicode_text=NULL WHERE id=NEW.id; END;
CREATE INDEX IF NOT EXISTS idx_status ON memories(status);
CREATE INDEX IF NOT EXISTS idx_kind ON memories(kind);
CREATE INDEX IF NOT EXISTS idx_scope ON memories(scope);
CREATE INDEX IF NOT EXISTS idx_importance ON memories(importance DESC);
CREATE INDEX IF NOT EXISTS idx_updated ON memories(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_status_importance_updated ON memories(status, importance DESC, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_status_updated_id ON memories(status, updated_at DESC, id DESC);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
  text, tags, audience, content='memories', content_rowid='rowid'
);
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts(rowid, text, tags, audience)
  VALUES (new.rowid,
    CASE WHEN json_valid(new.data) THEN json_extract(new.data, '$.text') ELSE '' END,
    COALESCE(new.tags, ''),
    COALESCE(new.audience, ''));
END;
CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, text, tags, audience)
  SELECT 'delete', old.rowid,
    json_extract(old.data, '$.text'),
    COALESCE(old.tags, ''),
    COALESCE(old.audience, '')
  WHERE json_valid(old.data);
END;
CREATE TRIGGER memories_au AFTER UPDATE ON memories
WHEN CASE WHEN json_valid(old.data) THEN json_extract(old.data, '$.text') END
       IS NOT CASE WHEN json_valid(new.data) THEN json_extract(new.data, '$.text') END
  OR COALESCE(old.tags, '') IS NOT COALESCE(new.tags, '')
  OR COALESCE(old.audience, '') IS NOT COALESCE(new.audience, '')
BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, text, tags, audience)
  SELECT 'delete', old.rowid,
    json_extract(old.data, '$.text'),
    COALESCE(old.tags, ''),
    COALESCE(old.audience, '')
  WHERE json_valid(old.data);
  INSERT INTO memories_fts(rowid, text, tags, audience)
  VALUES (new.rowid,
    CASE WHEN json_valid(new.data) THEN json_extract(new.data, '$.text') ELSE '' END,
    COALESCE(new.tags, ''),
    COALESCE(new.audience, ''));
END;
INSERT INTO schema_meta (key, value) VALUES ('fts_index_initialized', 1)
  ON CONFLICT(key) DO UPDATE SET value = 1;
CREATE TABLE IF NOT EXISTS edges (
  from_node TEXT NOT NULL,
  to_node TEXT NOT NULL,
  relation TEXT NOT NULL,
  weight REAL NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL,
  PRIMARY KEY (from_node, to_node, relation)
);
CREATE INDEX IF NOT EXISTS idx_edge_from ON edges(from_node);
CREATE INDEX IF NOT EXISTS idx_edge_to ON edges(to_node);
CREATE TABLE IF NOT EXISTS audit_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  event TEXT NOT NULL,
  at TEXT NOT NULL,
  trace_id TEXT,
  data TEXT
);
CREATE TABLE IF NOT EXISTS candidates (
  id TEXT PRIMARY KEY,
  data TEXT NOT NULL,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  canonical_text TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_candidates_status_created ON candidates(status, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_edge_to_relation ON edges(to_node, relation);
INSERT INTO schema_meta (key, value) VALUES ('version', 5) ON CONFLICT(key) DO NOTHING;
CREATE INDEX IF NOT EXISTS idx_canonical_text ON memories(canonical_text);
CREATE INDEX IF NOT EXISTS idx_status_scope_canonical ON memories(status, scope, canonical_text);
CREATE INDEX IF NOT EXISTS idx_legacy_scope ON memories(legacy_scope);
CREATE INDEX IF NOT EXISTS idx_owner_session ON memories(owner_session_id);
CREATE INDEX IF NOT EXISTS idx_candidates_status_canonical ON candidates(status, canonical_text);
"#;

/// HQ synchronization projection (`sqlite-store-hq-sync.ts`, schema 3).
/// Counter/verification-only changes are excluded from change detection.
const HQ_SYNC_DDL: &str = r#"
INSERT INTO schema_meta (key, value) VALUES ('hq_memory_sync_schema', 3)
  ON CONFLICT(key) DO UPDATE SET value = excluded.value;
CREATE TABLE IF NOT EXISTS hq_memory_sync (
  id TEXT PRIMARY KEY, revision INTEGER NOT NULL, change_id TEXT NOT NULL, data TEXT
);
CREATE TABLE IF NOT EXISTS hq_memory_sync_clock (
  id INTEGER PRIMARY KEY CHECK(id = 1), epoch TEXT NOT NULL, revision INTEGER NOT NULL
);
INSERT OR IGNORE INTO hq_memory_sync_clock VALUES(1, lower(hex(randomblob(16))), 0);
CREATE TRIGGER IF NOT EXISTS sage_hq_clock_insert AFTER INSERT ON hq_memory_sync BEGIN
  UPDATE hq_memory_sync_clock SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER IF NOT EXISTS sage_hq_clock_update AFTER UPDATE ON hq_memory_sync BEGIN
  UPDATE hq_memory_sync_clock SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER IF NOT EXISTS sage_hq_clock_delete AFTER DELETE ON hq_memory_sync BEGIN
  UPDATE hq_memory_sync_clock SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER IF NOT EXISTS sage_hq_insert AFTER INSERT ON memories WHEN new.scope IN ('project', 'file', 'symbol') AND json_valid(new.data) BEGIN
  INSERT INTO hq_memory_sync VALUES(new.id, 1, lower(hex(randomblob(16))), CASE WHEN json_valid(new.data) THEN json_remove(new.data, '$.injectionCount', '$.useCount', '$.lastAccessedAt', '$.lastUsedAt') ELSE NULL END)
  ON CONFLICT(id) DO UPDATE SET revision = revision + 1, change_id = excluded.change_id, data = excluded.data;
END;
CREATE TRIGGER IF NOT EXISTS sage_hq_update AFTER UPDATE ON memories
  WHEN (new.scope IN ('project', 'file', 'symbol') OR old.scope IN ('project', 'file', 'symbol')) AND json_valid(new.data) AND CASE WHEN json_valid(new.data) THEN json_remove(new.data, '$.injectionCount', '$.useCount', '$.lastAccessedAt', '$.lastUsedAt', '$.lastVerifiedAt', '$.freshness') ELSE NULL END IS NOT CASE WHEN json_valid(old.data) THEN json_remove(old.data, '$.injectionCount', '$.useCount', '$.lastAccessedAt', '$.lastUsedAt', '$.lastVerifiedAt', '$.freshness') ELSE NULL END BEGIN
  INSERT INTO hq_memory_sync VALUES(new.id, 1, lower(hex(randomblob(16))), CASE WHEN new.scope IN ('project', 'file', 'symbol') THEN CASE WHEN json_valid(new.data) THEN json_remove(new.data, '$.injectionCount', '$.useCount', '$.lastAccessedAt', '$.lastUsedAt') ELSE NULL END ELSE NULL END)
  ON CONFLICT(id) DO UPDATE SET revision = revision + 1, change_id = excluded.change_id, data = excluded.data;
END;
CREATE TRIGGER IF NOT EXISTS sage_hq_delete AFTER DELETE ON memories WHEN old.scope IN ('project', 'file', 'symbol') BEGIN
  INSERT INTO hq_memory_sync VALUES(old.id, 1, lower(hex(randomblob(16))), NULL)
  ON CONFLICT(id) DO UPDATE SET revision = revision + 1, change_id = excluded.change_id, data = NULL;
END;
"#;

/// Additive extension (`wrongmemory-ext-v1`). Its tables are ignored by
/// SAGE readers; the receipt table makes "crash after commit before reply"
/// recoverable (architecture §8). Applied only to stores the service owns.
const EXTENSION_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS wm_profile_extensions (
  name TEXT PRIMARY KEY,
  version INTEGER NOT NULL,
  applied_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS wm_mutation_receipts (
  principal_id TEXT NOT NULL,
  action TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  result TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (principal_id, action, idempotency_key)
);
"#;

pub const EXTENSION_NAME: &str = "wrongmemory-ext";
pub const EXTENSION_VERSION: i64 = 1;

/// Create a fresh SAGE v5 store in an empty database.
pub fn create_sage_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(CONNECTION_PRAGMAS)?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = conn
        .execute_batch(CORE_DDL)
        .and_then(|_| conn.execute_batch(HQ_SYNC_DDL));
    finish(conn, result)
}

/// Apply the WrongMemory extension inside one transaction.
pub fn apply_extension(conn: &Connection, now_iso: &str) -> rusqlite::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = conn.execute_batch(EXTENSION_DDL).and_then(|_| {
        conn.execute(
            "INSERT INTO wm_profile_extensions(name, version, applied_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO NOTHING",
            rusqlite::params![EXTENSION_NAME, EXTENSION_VERSION, now_iso],
        )
        .map(|_| ())
    });
    finish(conn, result)
}

/// Commit or roll back, preserving the primary error: after SQLITE_FULL/IOERR
/// SQLite may already have rolled back, and a failing ROLLBACK must not mask it.
pub fn finish<T>(conn: &Connection, result: rusqlite::Result<T>) -> rusqlite::Result<T> {
    match result {
        Ok(v) => {
            conn.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            if !conn.is_autocommit() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            Err(e)
        }
    }
}

/// `sageTextIndex.revision` for `memories.unicode_text`.
pub const TEXT_INDEX_REVISION: &str = "nfkc-lower-v1";
