//! Read-only store inspection and profile admission (architecture §6).
//!
//! Opens with `SQLITE_OPEN_READ_ONLY` and `query_only`: no initializer,
//! migration, trigger repair, or text-index refresh runs here. Unknown or
//! newer stores are reported, never "fixed".

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::schema::{CANDIDATE_COLUMNS, EDGE_COLUMNS, MEMORY_COLUMNS, SAGE_TABLES};
use wrongmemory_contracts::SAGE_SQLITE_SCHEMA_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// v5 SAGE store with the expected shape.
    Compatible,
    /// v5 store carrying tables this build does not know: readable, not writable.
    UnknownData,
    /// Older SAGE schema that SAGE itself would migrate first. Not admitted:
    /// session ownership columns may be missing, so visibility cannot be enforced.
    Older,
    /// Newer schema version than this build supports.
    Newer,
    /// Claims v5 but required columns/tables are missing.
    Divergent,
    /// A SQLite database that is not a SAGE store.
    NotASageStore,
    Unreadable,
    Missing,
}

impl Verdict {
    pub fn readable(self) -> bool {
        matches!(self, Verdict::Compatible | Verdict::UnknownData)
    }
    pub fn writable(self) -> bool {
        matches!(self, Verdict::Compatible)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub path: String,
    pub verdict: Verdict,
    pub detail: String,
    pub schema_meta: BTreeMap<String, i64>,
    pub text_index_revisions: BTreeMap<String, String>,
    pub tables: Vec<String>,
    pub unknown_tables: Vec<String>,
    pub missing: Vec<String>,
    pub fts_present: bool,
    pub unicode_text_column: bool,
    pub wrongmemory_extension: Option<i64>,
    pub memory_rows: Option<i64>,
    pub candidate_rows: Option<i64>,
    pub edge_rows: Option<i64>,
    /// `server.json` beside the store: a legacy SAGE project daemon may own it.
    pub legacy_owner_metadata: bool,
    pub sqlite_version: String,
}

fn base(path: &Path, verdict: Verdict, detail: impl Into<String>) -> Inspection {
    Inspection {
        path: path.display().to_string(),
        verdict,
        detail: detail.into(),
        schema_meta: BTreeMap::new(),
        text_index_revisions: BTreeMap::new(),
        tables: vec![],
        unknown_tables: vec![],
        missing: vec![],
        fts_present: false,
        unicode_text_column: false,
        wrongmemory_extension: None,
        memory_rows: None,
        candidate_rows: None,
        edge_rows: None,
        legacy_owner_metadata: legacy_owner_metadata(path),
        sqlite_version: rusqlite::version().to_string(),
    }
}

/// True when SAGE's project-server metadata exists next to the store.
pub fn legacy_owner_metadata(store: &Path) -> bool {
    store
        .parent()
        .map(|d| d.join("server.json").exists())
        .unwrap_or(false)
}

pub fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

pub fn inspect(path: &Path) -> Inspection {
    if !path.exists() {
        return base(path, Verdict::Missing, "store file does not exist");
    }
    let conn = match open_read_only(path) {
        Ok(c) => c,
        Err(e) => return base(path, Verdict::Unreadable, e.to_string()),
    };
    match inspect_conn(path, &conn) {
        Ok(i) => i,
        Err(e) => {
            let msg = e.to_string();
            let verdict = if msg.contains("not a database") {
                Verdict::NotASageStore
            } else {
                Verdict::Unreadable
            };
            base(path, verdict, msg)
        }
    }
}

fn columns(conn: &Connection, table: &str) -> rusqlite::Result<BTreeSet<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_xinfo(?1)")?;
    let rows = stmt.query_map([table], |r| r.get::<_, String>(0))?;
    rows.collect()
}

fn known_table(name: &str) -> bool {
    SAGE_TABLES.contains(&name)
        || name.starts_with("memories_fts_")
        || name.starts_with("sqlite_")
        || name.starts_with("wm_")
}

pub fn inspect_conn(path: &Path, conn: &Connection) -> rusqlite::Result<Inspection> {
    let mut out = base(path, Verdict::Compatible, "");
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
    out.tables = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let has = |t: &str| out.tables.iter().any(|x| x == t);
    if !has("memories") || !has("schema_meta") {
        out.verdict = Verdict::NotASageStore;
        out.detail = "no memories/schema_meta tables".into();
        return Ok(out);
    }
    {
        let mut stmt = conn.prepare("SELECT key, value FROM schema_meta")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (k, v) = row?;
            out.schema_meta.insert(k, v);
        }
    }
    if has("ws_text_index_revisions") {
        let mut stmt = conn.prepare("SELECT name, revision FROM ws_text_index_revisions")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (k, v) = row?;
            out.text_index_revisions.insert(k, v);
        }
    }
    if has("wm_profile_extensions") {
        out.wrongmemory_extension = conn
            .query_row(
                "SELECT version FROM wm_profile_extensions WHERE name = 'wrongmemory-ext'",
                [],
                |r| r.get(0),
            )
            .ok();
    }
    out.fts_present = has("memories_fts");
    out.unknown_tables = out
        .tables
        .iter()
        .filter(|t| !known_table(t))
        .cloned()
        .collect();
    let mem_cols = columns(conn, "memories")?;
    out.unicode_text_column = mem_cols.contains("unicode_text");
    for c in MEMORY_COLUMNS {
        if !mem_cols.contains(*c) {
            out.missing.push(format!("memories.{c}"));
        }
    }
    for (table, required) in [("candidates", CANDIDATE_COLUMNS), ("edges", EDGE_COLUMNS)] {
        if !has(table) {
            out.missing.push(table.to_string());
            continue;
        }
        let cols = columns(conn, table)?;
        for c in required {
            if !cols.contains(*c) {
                out.missing.push(format!("{table}.{c}"));
            }
        }
    }
    let count = |t: &str| -> Option<i64> {
        conn.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0))
            .ok()
    };
    out.memory_rows = count("memories");
    if has("candidates") {
        out.candidate_rows = count("candidates");
    }
    if has("edges") {
        out.edge_rows = count("edges");
    }

    let version = out.schema_meta.get("version").copied();
    (out.verdict, out.detail) = match version {
        Some(v) if v > SAGE_SQLITE_SCHEMA_VERSION => (
            Verdict::Newer,
            format!("schema version {v} is newer than supported {SAGE_SQLITE_SCHEMA_VERSION}"),
        ),
        Some(v) if v < SAGE_SQLITE_SCHEMA_VERSION => (
            Verdict::Older,
            format!(
                "schema version {v} predates {SAGE_SQLITE_SCHEMA_VERSION}; open it with SAGE once to migrate"
            ),
        ),
        None => (
            Verdict::Divergent,
            "schema_meta has no version key".to_string(),
        ),
        Some(_) if !out.missing.is_empty() => (
            Verdict::Divergent,
            format!("missing: {}", out.missing.join(", ")),
        ),
        Some(_) if !out.unknown_tables.is_empty() => (
            Verdict::UnknownData,
            format!("unknown tables: {}", out.unknown_tables.join(", ")),
        ),
        Some(_) => (Verdict::Compatible, "SAGE SQLite schema v5".to_string()),
    };
    if let Some(rev) = out.text_index_revisions.get("memories.unicode_text")
        && rev != "nfkc-lower-v1"
        && out.verdict == Verdict::Compatible
    {
        out.verdict = Verdict::UnknownData;
        out.detail = format!("unknown unicode_text revision {rev}");
    }
    Ok(out)
}
