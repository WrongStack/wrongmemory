//! `registry.db`: service-owned state (architecture §5). It never holds
//! memory content; SAGE stores stay authoritative (WM-INV-04).
//!
//! Tables: `registry_meta`, `projects`, `client_grants`,
//! `ownership_transitions`. Jobs/schedules/event offsets arrive with M2.

pub mod jobs;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use wrongmemory_contracts::{
    Admission, ErrorBody, ErrorCode, GrantInfo, PolicyProfile, ProjectInfo, StoreKind,
};

pub const REGISTRY_SCHEMA_VERSION: i64 = 2;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// RFC 3339 UTC with millisecond precision (JS `Date#toISOString`).
pub fn iso_from_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

pub fn now_iso() -> String {
    iso_from_ms(now_ms())
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("registry schema {found} is newer than supported {supported}")]
    Newer { found: i64, supported: i64 },
}

impl From<RegistryError> for ErrorBody {
    fn from(e: RegistryError) -> Self {
        ErrorBody::new(ErrorCode::Internal, e.to_string())
    }
}

use jobs::MIGRATION_2;

const MIGRATION_1: &str = r#"
CREATE TABLE registry_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE projects (
  project_id TEXT PRIMARY KEY,
  display_name TEXT NOT NULL,
  root TEXT NOT NULL,
  root_key TEXT NOT NULL,
  store_path TEXT NOT NULL,
  store_key TEXT NOT NULL UNIQUE,
  store_kind TEXT NOT NULL,
  want_write INTEGER NOT NULL,
  admission TEXT NOT NULL,
  admission_reason TEXT NOT NULL,
  storage_profile_id TEXT,
  owner_epoch INTEGER NOT NULL DEFAULT 0,
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_projects_root ON projects(root_key);
CREATE TABLE client_grants (
  grant_id TEXT PRIMARY KEY,
  label TEXT NOT NULL,
  secret_sha256 TEXT NOT NULL,
  profile TEXT NOT NULL,
  projects TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  expires_at_ms INTEGER,
  revoked_at_ms INTEGER
);
CREATE TABLE ownership_transitions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  phase TEXT NOT NULL,
  instance_id TEXT NOT NULL,
  detail TEXT,
  at_ms INTEGER NOT NULL
);
"#;

pub struct Registry {
    pub(crate) conn: Connection,
}

/// Case/separator-insensitive key for a filesystem path on Windows; exact on
/// other platforms. Display paths are stored separately.
pub fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

pub struct NewProject<'a> {
    pub display_name: &'a str,
    pub root: &'a Path,
    pub store_path: &'a Path,
    pub store_kind: StoreKind,
    pub want_write: bool,
    pub admission: Admission,
    pub admission_reason: &'a str,
    pub storage_profile_id: Option<&'a str>,
}

impl Registry {
    pub fn open(path: &Path) -> Result<Self, RegistryError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA busy_timeout = 10000; PRAGMA foreign_keys = ON;",
        )?;
        let mut reg = Self { conn };
        reg.migrate()?;
        Ok(reg)
    }

    fn version(&self) -> Result<i64, RegistryError> {
        let has: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE name = 'registry_meta'",
            [],
            |r| r.get(0),
        )?;
        if !has {
            return Ok(0);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM registry_meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    fn migrate(&mut self) -> Result<(), RegistryError> {
        let v = self.version()?;
        if v > REGISTRY_SCHEMA_VERSION {
            return Err(RegistryError::Newer {
                found: v,
                supported: REGISTRY_SCHEMA_VERSION,
            });
        }
        if v < 1 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(MIGRATION_1)?;
            tx.execute(
                "INSERT INTO registry_meta(key, value) VALUES ('schema_version', '1')",
                [],
            )?;
            tx.commit()?;
        }
        if v < 2 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(MIGRATION_2)?;
            tx.execute(
                "UPDATE registry_meta SET value = '2' WHERE key = 'schema_version'",
                [],
            )?;
            tx.commit()?;
        }
        jobs::upgrade_default_cadence(&mut self.conn)
    }

    // ─── Semantic provider (WM-508) ─────────────────────────────────────────
    // Kept in `registry_meta` (key `semantic.provider:<projectId>`), so the
    // registry schema does not change; absent means `none`.

    pub fn semantic_provider(&self, project_id: &str) -> Result<Option<String>, RegistryError> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM registry_meta WHERE key = ?1",
                [format!("semantic.provider:{project_id}")],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    pub fn set_semantic_provider(
        &mut self,
        project_id: &str,
        provider: Option<&str>,
    ) -> Result<(), RegistryError> {
        let key = format!("semantic.provider:{project_id}");
        match provider {
            Some(p) => self.conn.execute(
                "INSERT OR REPLACE INTO registry_meta(key, value) VALUES (?1, ?2)",
                [key.as_str(), p],
            )?,
            None => self
                .conn
                .execute("DELETE FROM registry_meta WHERE key = ?1", [key.as_str()])?,
        };
        Ok(())
    }

    // ─── Projects ───────────────────────────────────────────────────────────

    pub fn find_project_by_store(
        &self,
        store: &Path,
    ) -> Result<Option<ProjectInfo>, RegistryError> {
        self.project_where("store_key = ?1", &path_key(store))
    }

    pub fn find_project_by_root(&self, root: &Path) -> Result<Option<ProjectInfo>, RegistryError> {
        self.project_where("root_key = ?1", &path_key(root))
    }

    pub fn get_project(&self, id: &str) -> Result<Option<ProjectInfo>, RegistryError> {
        self.project_where("project_id = ?1", id)
    }

    /// `want_write` as requested at registration.
    pub fn wants_write(&self, id: &str) -> Result<bool, RegistryError> {
        Ok(self
            .conn
            .query_row(
                "SELECT want_write FROM projects WHERE project_id = ?1",
                [id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0)
    }

    fn project_where(&self, clause: &str, arg: &str) -> Result<Option<ProjectInfo>, RegistryError> {
        Ok(self
            .conn
            .query_row(
                &format!("{PROJECT_SELECT} WHERE enabled = 1 AND {clause}"),
                [arg],
                row_to_project,
            )
            .optional()?)
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectInfo>, RegistryError> {
        let mut stmt = self.conn.prepare(&format!(
            "{PROJECT_SELECT} WHERE enabled = 1 ORDER BY created_at_ms, project_id"
        ))?;
        let rows = stmt.query_map([], row_to_project)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn insert_project(&mut self, p: &NewProject<'_>) -> Result<ProjectInfo, RegistryError> {
        let id = format!("p_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        self.insert_project_with_id(&id, p)
    }

    pub fn insert_project_with_id(
        &mut self,
        id: &str,
        p: &NewProject<'_>,
    ) -> Result<ProjectInfo, RegistryError> {
        self.conn.execute(
            "INSERT INTO projects (project_id, display_name, root, root_key, store_path, store_key,
               store_kind, want_write, admission, admission_reason, storage_profile_id, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                id,
                p.display_name,
                p.root.to_string_lossy(),
                path_key(p.root),
                p.store_path.to_string_lossy(),
                path_key(p.store_path),
                kind_str(p.store_kind),
                p.want_write as i64,
                admission_str(p.admission),
                p.admission_reason,
                p.storage_profile_id,
                now_ms(),
            ],
        )?;
        Ok(self.get_project(id)?.expect("inserted project"))
    }

    pub fn set_admission(
        &mut self,
        id: &str,
        admission: Admission,
        reason: &str,
        profile: Option<&str>,
    ) -> Result<(), RegistryError> {
        self.conn.execute(
            "UPDATE projects SET admission = ?2, admission_reason = ?3, storage_profile_id = ?4
              WHERE project_id = ?1",
            params![id, admission_str(admission), reason, profile],
        )?;
        Ok(())
    }

    /// Increment the owner epoch and journal the activation (architecture §7).
    pub fn begin_ownership(&mut self, id: &str, instance: &str) -> Result<i64, RegistryError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE projects SET owner_epoch = owner_epoch + 1 WHERE project_id = ?1",
            [id],
        )?;
        let epoch: i64 = tx.query_row(
            "SELECT owner_epoch FROM projects WHERE project_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO ownership_transitions(project_id, epoch, phase, instance_id, at_ms)
             VALUES (?1, ?2, 'activated', ?3, ?4)",
            params![id, epoch, instance, now_ms()],
        )?;
        tx.commit()?;
        Ok(epoch)
    }

    pub fn record_release(
        &mut self,
        id: &str,
        epoch: i64,
        instance: &str,
    ) -> Result<(), RegistryError> {
        self.conn.execute(
            "INSERT INTO ownership_transitions(project_id, epoch, phase, instance_id, at_ms)
             VALUES (?1, ?2, 'released', ?3, ?4)",
            params![id, epoch, instance, now_ms()],
        )?;
        Ok(())
    }

    /// Whether the latest ownership record of `id` is an activation by
    /// another instance of this data root that has not recorded its release:
    /// a predecessor daemon that is still shutting down. A store held by a
    /// daemon of another data root has no such record here.
    pub fn predecessor_holds(&self, id: &str, instance: &str) -> Result<bool, RegistryError> {
        let last: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT phase, instance_id FROM ownership_transitions
                  WHERE project_id = ?1 ORDER BY id DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(last.is_some_and(|(phase, holder)| phase == "activated" && holder != instance))
    }

    /// Detach: the registration is disabled; the store is never deleted.
    pub fn detach_project(&mut self, id: &str) -> Result<bool, RegistryError> {
        Ok(self.conn.execute(
            "UPDATE projects SET enabled = 0, store_key = store_key || '#detached-' || ?2
              WHERE project_id = ?1 AND enabled = 1",
            params![id, now_ms()],
        )? > 0)
    }

    // ─── Grants ─────────────────────────────────────────────────────────────

    pub fn create_grant(
        &mut self,
        label: &str,
        profile: PolicyProfile,
        projects: &[String],
        expires_at_ms: Option<i64>,
        secret: &str,
    ) -> Result<GrantInfo, RegistryError> {
        let id = format!("g_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        self.conn.execute(
            "INSERT INTO client_grants (grant_id, label, secret_sha256, profile, projects,
               created_at_ms, expires_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                label,
                secret_hash(secret),
                profile.as_str(),
                serde_json::to_string(projects).unwrap_or_else(|_| "[]".into()),
                now_ms(),
                expires_at_ms,
            ],
        )?;
        Ok(self.get_grant(&id)?.expect("inserted grant"))
    }

    pub fn get_grant(&self, id: &str) -> Result<Option<GrantInfo>, RegistryError> {
        Ok(self
            .conn
            .query_row(
                &format!("{GRANT_SELECT} WHERE grant_id = ?1"),
                [id],
                row_to_grant,
            )
            .optional()?)
    }

    pub fn list_grants(&self) -> Result<Vec<GrantInfo>, RegistryError> {
        let mut stmt = self
            .conn
            .prepare(&format!("{GRANT_SELECT} ORDER BY created_at_ms, grant_id"))?;
        let rows = stmt.query_map([], row_to_grant)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn revoke_grant(&mut self, id: &str) -> Result<bool, RegistryError> {
        Ok(self.conn.execute(
            "UPDATE client_grants SET revoked_at_ms = ?2 WHERE grant_id = ?1 AND revoked_at_ms IS NULL",
            params![id, now_ms()],
        )? > 0)
    }

    /// Verify a grant credential. Unknown, revoked, expired and wrong-secret
    /// all return `None` (indistinguishable to the caller).
    pub fn authenticate_grant(
        &self,
        id: &str,
        secret: &str,
    ) -> Result<Option<GrantInfo>, RegistryError> {
        let row: Option<(String, Option<i64>, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT secret_sha256, expires_at_ms, revoked_at_ms FROM client_grants WHERE grant_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((hash, expires, revoked)) = row else {
            // Equalize work for absent IDs.
            let _ = secret_hash(secret);
            return Ok(None);
        };
        let ok = constant_eq(hash.as_bytes(), secret_hash(secret).as_bytes())
            && revoked.is_none()
            && expires.is_none_or(|e| e > now_ms());
        if ok { self.get_grant(id) } else { Ok(None) }
    }

    /// Whether a grant is still usable (revocation is checked per request).
    pub fn grant_active(&self, id: &str) -> Result<bool, RegistryError> {
        Ok(self
            .get_grant(id)?
            .map(|g| g.revoked_at_ms.is_none() && g.expires_at_ms.is_none_or(|e| e > now_ms()))
            .unwrap_or(false))
    }
}

pub fn secret_hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const PROJECT_SELECT: &str = "SELECT project_id, display_name, root, store_path, store_kind,
  admission, admission_reason, owner_epoch, storage_profile_id, created_at_ms FROM projects";

const GRANT_SELECT: &str = "SELECT grant_id, label, profile, projects, created_at_ms,
  expires_at_ms, revoked_at_ms FROM client_grants";

fn kind_str(k: StoreKind) -> &'static str {
    match k {
        StoreKind::Adopted => "adopted",
        StoreKind::Managed => "managed",
    }
}

fn admission_str(a: Admission) -> &'static str {
    match a {
        Admission::ReadWrite => "read-write",
        Admission::ReadOnly => "read-only",
        Admission::Refused => "refused",
    }
}

fn row_to_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectInfo> {
    let kind: String = r.get(4)?;
    let admission: String = r.get(5)?;
    Ok(ProjectInfo {
        project_id: r.get(0)?,
        display_name: r.get(1)?,
        root: r.get(2)?,
        store_path: r.get(3)?,
        store_kind: if kind == "managed" {
            StoreKind::Managed
        } else {
            StoreKind::Adopted
        },
        admission: match admission.as_str() {
            "read-write" => Admission::ReadWrite,
            "read-only" => Admission::ReadOnly,
            _ => Admission::Refused,
        },
        admission_reason: r.get(6)?,
        owner_epoch: r.get(7)?,
        storage_profile_id: r.get(8)?,
        created_at_ms: r.get(9)?,
    })
}

fn row_to_grant(r: &rusqlite::Row<'_>) -> rusqlite::Result<GrantInfo> {
    let profile: String = r.get(2)?;
    let projects: String = r.get(3)?;
    Ok(GrantInfo {
        grant_id: r.get(0)?,
        label: r.get(1)?,
        profile: PolicyProfile::parse(&profile).unwrap_or(PolicyProfile::ReadOnly),
        projects: serde_json::from_str(&projects).unwrap_or_default(),
        created_at_ms: r.get(4)?,
        expires_at_ms: r.get(5)?,
        revoked_at_ms: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> (tempfile::TempDir, Registry) {
        let tmp = tempfile::tempdir().unwrap();
        let r = Registry::open(&tmp.path().join("registry.db")).unwrap();
        (tmp, r)
    }

    #[test]
    fn iso_formatting_matches_js() {
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(1_793_534_400_123), "2026-11-01T12:00:00.123Z");
        assert_eq!(iso_from_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn predecessor_holds_only_after_an_unreleased_activation_by_another_instance() {
        let (_t, mut r) = reg();
        let a = r
            .insert_project(&NewProject {
                display_name: "a",
                root: Path::new("C:/Work/A"),
                store_path: Path::new("C:/Work/A/store/sage.db"),
                store_kind: StoreKind::Managed,
                want_write: true,
                admission: Admission::ReadWrite,
                admission_reason: "test",
                storage_profile_id: None,
            })
            .unwrap()
            .project_id;
        // Never owned here: a lock holder must be foreign (no wait).
        assert!(!r.predecessor_holds(&a, "d_new").unwrap());
        let epoch = r.begin_ownership(&a, "d_old").unwrap();
        assert!(r.predecessor_holds(&a, "d_new").unwrap());
        assert!(!r.predecessor_holds(&a, "d_old").unwrap(), "not our own");
        r.record_release(&a, epoch, "d_old").unwrap();
        assert!(!r.predecessor_holds(&a, "d_new").unwrap(), "released");
    }

    #[test]
    fn store_identity_is_unique_and_case_insensitive_on_windows() {
        let (_t, mut r) = reg();
        let p = NewProject {
            display_name: "a",
            root: Path::new("C:/Work/A"),
            store_path: Path::new("C:/Work/A/.wrongstack/memories/sage.db"),
            store_kind: StoreKind::Adopted,
            want_write: false,
            admission: Admission::ReadOnly,
            admission_reason: "test",
            storage_profile_id: None,
        };
        let a = r.insert_project(&p).unwrap();
        assert!(
            r.insert_project(&p).is_err(),
            "same store cannot register twice"
        );
        if cfg!(windows) {
            let found = r
                .find_project_by_store(Path::new("c:\\work\\a\\.wrongstack\\memories\\sage.db"))
                .unwrap()
                .unwrap();
            assert_eq!(found.project_id, a.project_id);
        }
        assert!(r.detach_project(&a.project_id).unwrap());
        assert!(r.get_project(&a.project_id).unwrap().is_none());
        r.insert_project(&p).unwrap();
    }

    #[test]
    fn grants_authenticate_and_revoke() {
        let (_t, mut r) = reg();
        let g = r
            .create_grant(
                "codex",
                PolicyProfile::ReadPropose,
                &["p1".into()],
                None,
                "s3cret",
            )
            .unwrap();
        assert!(
            r.authenticate_grant(&g.grant_id, "s3cret")
                .unwrap()
                .is_some()
        );
        assert!(
            r.authenticate_grant(&g.grant_id, "wrong")
                .unwrap()
                .is_none()
        );
        assert!(
            r.authenticate_grant("g_missing", "s3cret")
                .unwrap()
                .is_none()
        );
        assert!(r.revoke_grant(&g.grant_id).unwrap());
        assert!(
            r.authenticate_grant(&g.grant_id, "s3cret")
                .unwrap()
                .is_none()
        );
        assert!(!r.grant_active(&g.grant_id).unwrap());
        let expired = r
            .create_grant("old", PolicyProfile::ReadOnly, &[], Some(1), "x")
            .unwrap();
        assert!(
            r.authenticate_grant(&expired.grant_id, "x")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn epochs_increase() {
        let (_t, mut r) = reg();
        let p = r
            .insert_project(&NewProject {
                display_name: "a",
                root: Path::new("/a"),
                store_path: Path::new("/a/s.db"),
                store_kind: StoreKind::Managed,
                want_write: true,
                admission: Admission::ReadWrite,
                admission_reason: "",
                storage_profile_id: None,
            })
            .unwrap();
        assert_eq!(r.begin_ownership(&p.project_id, "i1").unwrap(), 1);
        assert_eq!(r.begin_ownership(&p.project_id, "i2").unwrap(), 2);
    }

    /// WM-701: a registry written by the v1 build (no jobs/schedules tables)
    /// upgrades in place; projects, ownership epochs and grants survive.
    #[test]
    fn v1_registry_upgrades_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.db");
        let (project, grant) = {
            let mut r = Registry::open(&path).unwrap();
            let p = r
                .insert_project(&NewProject {
                    display_name: "a",
                    root: Path::new("/a"),
                    store_path: Path::new("/a/s.db"),
                    store_kind: StoreKind::Managed,
                    want_write: true,
                    admission: Admission::ReadWrite,
                    admission_reason: "",
                    storage_profile_id: None,
                })
                .unwrap();
            r.begin_ownership(&p.project_id, "i1").unwrap();
            let g = r
                .create_grant(
                    "codex",
                    PolicyProfile::Writer,
                    std::slice::from_ref(&p.project_id),
                    None,
                    "s3cret",
                )
                .unwrap();
            (p, g)
        };
        // Reduce it to exactly what the v1 build wrote.
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "DROP TABLE jobs; DROP TABLE maintenance_schedules;
                 UPDATE registry_meta SET value = '1' WHERE key = 'schema_version';",
            )
            .unwrap();
        }
        let mut r = Registry::open(&path).unwrap();
        assert_eq!(r.version().unwrap(), REGISTRY_SCHEMA_VERSION);
        let got = r.get_project(&project.project_id).unwrap().unwrap();
        assert_eq!(got.store_path, project.store_path);
        assert_eq!(r.begin_ownership(&project.project_id, "i2").unwrap(), 2);
        let auth = r
            .authenticate_grant(&grant.grant_id, "s3cret")
            .unwrap()
            .unwrap();
        assert_eq!(auth.profile, PolicyProfile::Writer);
        let c = Connection::open(&path).unwrap();
        for t in ["jobs", "maintenance_schedules"] {
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [t],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{t} recreated");
        }
        // Reopening an up-to-date registry is a no-op.
        drop(r);
        assert_eq!(
            Registry::open(&path).unwrap().version().unwrap(),
            REGISTRY_SCHEMA_VERSION
        );
    }

    #[test]
    fn newer_registry_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.db");
        drop(Registry::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE registry_meta SET value = '99' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        assert!(matches!(
            Registry::open(&path),
            Err(RegistryError::Newer { .. })
        ));
    }
}
