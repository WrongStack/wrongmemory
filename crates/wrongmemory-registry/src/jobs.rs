//! Registry migration 2: durable maintenance schedules and jobs
//! (architecture §5, §10; WM-206). Jobs record intent and outcome; the SAGE
//! store stays authoritative for knowledge.

use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use wrongmemory_contracts::{JobInfo, ScheduleInfo};

use crate::{Registry, RegistryError, now_ms};

pub(crate) const MIGRATION_2: &str = r#"
CREATE TABLE maintenance_schedules (
  project_id TEXT PRIMARY KEY,
  cadence_ms INTEGER NOT NULL,
  options_json TEXT NOT NULL DEFAULT '{}',
  options_revision INTEGER NOT NULL DEFAULT 1,
  next_due_ms INTEGER NOT NULL,
  last_run_ms INTEGER,
  last_outcome TEXT,
  enabled INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE jobs (
  job_id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  state TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 0,
  options_json TEXT NOT NULL DEFAULT '{}',
  created_at_ms INTEGER NOT NULL,
  started_at_ms INTEGER,
  finished_at_ms INTEGER,
  result_json TEXT,
  error_json TEXT,
  attempt INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_jobs_state ON jobs(state, priority DESC, created_at_ms);
CREATE INDEX idx_jobs_project ON jobs(project_id, created_at_ms DESC);
"#;

/// Default maintenance cadence: once a day.
pub const DEFAULT_CADENCE_MS: i64 = 24 * 60 * 60 * 1000;
/// At most this many queued jobs; further admissions coalesce or are refused.
pub const MAX_QUEUED_JOBS: i64 = 32;

const SCHEDULE_SELECT: &str =
    "SELECT project_id, enabled, cadence_ms, options_json, options_revision,
  next_due_ms, last_run_ms, last_outcome FROM maintenance_schedules";
const JOB_SELECT: &str = "SELECT job_id, project_id, kind, state, priority, options_json,
  created_at_ms, started_at_ms, finished_at_ms, result_json, error_json, attempt FROM jobs";

fn json_col(s: Option<String>) -> Option<Value> {
    s.and_then(|s| serde_json::from_str(&s).ok())
}

fn row_to_schedule(r: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleInfo> {
    Ok(ScheduleInfo {
        project_id: r.get(0)?,
        enabled: r.get::<_, i64>(1)? != 0,
        cadence_ms: r.get(2)?,
        options: json_col(r.get(3)?).unwrap_or_else(|| json!({})),
        options_revision: r.get(4)?,
        next_due_ms: r.get(5)?,
        last_run_ms: r.get(6)?,
        last_outcome: r.get(7)?,
    })
}

fn row_to_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobInfo> {
    Ok(JobInfo {
        job_id: r.get(0)?,
        project_id: r.get(1)?,
        kind: r.get(2)?,
        state: r.get(3)?,
        priority: r.get(4)?,
        options: json_col(r.get(5)?).unwrap_or_else(|| json!({})),
        created_at_ms: r.get(6)?,
        started_at_ms: r.get(7)?,
        finished_at_ms: r.get(8)?,
        result: json_col(r.get(9)?),
        error: json_col(r.get(10)?),
        attempt: r.get(11)?,
    })
}

/// Outcome of [`Registry::admit_job`].
#[derive(Debug, Clone)]
pub enum JobAdmission {
    /// A new job was queued.
    Queued(JobInfo),
    /// A queued or running job of the same kind already exists (single-flight).
    Coalesced(JobInfo),
    /// The global queue is full.
    Saturated,
}

impl Registry {
    pub fn get_schedule(&self, project_id: &str) -> Result<Option<ScheduleInfo>, RegistryError> {
        Ok(self
            .conn
            .query_row(
                &format!("{SCHEDULE_SELECT} WHERE project_id = ?1"),
                [project_id],
                row_to_schedule,
            )
            .optional()?)
    }

    /// Create the default schedule if absent (first run one cadence later).
    pub fn ensure_schedule(&mut self, project_id: &str) -> Result<ScheduleInfo, RegistryError> {
        self.conn.execute(
            "INSERT INTO maintenance_schedules (project_id, cadence_ms, next_due_ms)
             VALUES (?1, ?2, ?3) ON CONFLICT(project_id) DO NOTHING",
            params![
                project_id,
                DEFAULT_CADENCE_MS,
                now_ms() + DEFAULT_CADENCE_MS
            ],
        )?;
        Ok(self.get_schedule(project_id)?.expect("schedule exists"))
    }

    /// Update cadence/options/enabled. Options changes bump the revision.
    /// An `expected_revision` mismatch returns `Ok(Err(current))`.
    pub fn configure_schedule(
        &mut self,
        project_id: &str,
        cadence_ms: Option<i64>,
        options: Option<&Value>,
        enabled: Option<bool>,
        expected_revision: Option<i64>,
    ) -> Result<Result<ScheduleInfo, ScheduleInfo>, RegistryError> {
        let current = self.ensure_schedule(project_id)?;
        if let Some(e) = expected_revision
            && e != current.options_revision
        {
            return Ok(Err(current));
        }
        let tx = self.conn.transaction()?;
        if let Some(c) = cadence_ms {
            tx.execute(
                "UPDATE maintenance_schedules SET cadence_ms = ?2,
                   next_due_ms = MIN(next_due_ms, ?3) WHERE project_id = ?1",
                params![project_id, c, now_ms() + c],
            )?;
        }
        if let Some(o) = options {
            tx.execute(
                "UPDATE maintenance_schedules SET options_json = ?2,
                   options_revision = options_revision + 1 WHERE project_id = ?1",
                params![project_id, o.to_string()],
            )?;
        }
        if let Some(e) = enabled {
            tx.execute(
                "UPDATE maintenance_schedules SET enabled = ?2 WHERE project_id = ?1",
                params![project_id, e as i64],
            )?;
        }
        tx.commit()?;
        Ok(Ok(self.get_schedule(project_id)?.expect("schedule exists")))
    }

    /// Enabled schedules due at `now`.
    pub fn due_schedules(&self, now: i64) -> Result<Vec<ScheduleInfo>, RegistryError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SCHEDULE_SELECT} WHERE enabled = 1 AND next_due_ms <= ?1 ORDER BY next_due_ms, project_id"
        ))?;
        let rows = stmt.query_map([now], row_to_schedule)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_next_due(
        &mut self,
        project_id: &str,
        next_due_ms: i64,
    ) -> Result<(), RegistryError> {
        self.conn.execute(
            "UPDATE maintenance_schedules SET next_due_ms = ?2 WHERE project_id = ?1",
            params![project_id, next_due_ms],
        )?;
        Ok(())
    }

    /// Record a finished run and schedule the next one a cadence later.
    pub fn record_schedule_run(
        &mut self,
        project_id: &str,
        outcome: &str,
    ) -> Result<(), RegistryError> {
        let now = now_ms();
        self.conn.execute(
            "UPDATE maintenance_schedules SET last_run_ms = ?2, last_outcome = ?3,
               next_due_ms = ?2 + cadence_ms WHERE project_id = ?1",
            params![project_id, now, outcome],
        )?;
        Ok(())
    }

    /// Queue a job unless one of the same kind is already queued/running for
    /// the project (single-flight) or the global queue is full.
    pub fn admit_job(
        &mut self,
        project_id: &str,
        kind: &str,
        priority: i64,
        options: &Value,
    ) -> Result<JobAdmission, RegistryError> {
        let tx = self.conn.transaction()?;
        let existing = tx
            .query_row(
                &format!(
                    "{JOB_SELECT} WHERE project_id = ?1 AND kind = ?2 AND state IN ('queued','running')
                     ORDER BY created_at_ms LIMIT 1"
                ),
                params![project_id, kind],
                row_to_job,
            )
            .optional()?;
        if let Some(j) = existing {
            if j.state == "queued" && priority > j.priority {
                tx.execute(
                    "UPDATE jobs SET priority = ?2 WHERE job_id = ?1",
                    params![j.job_id, priority],
                )?;
            }
            tx.commit()?;
            return Ok(JobAdmission::Coalesced(j));
        }
        let queued: i64 = tx.query_row(
            "SELECT COUNT(*) FROM jobs WHERE state = 'queued'",
            [],
            |r| r.get(0),
        )?;
        if queued >= MAX_QUEUED_JOBS {
            tx.commit()?;
            return Ok(JobAdmission::Saturated);
        }
        let id = format!("j_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        tx.execute(
            "INSERT INTO jobs (job_id, project_id, kind, state, priority, options_json, created_at_ms)
             VALUES (?1, ?2, ?3, 'queued', ?4, ?5, ?6)",
            params![id, project_id, kind, priority, options.to_string(), now_ms()],
        )?;
        tx.commit()?;
        Ok(JobAdmission::Queued(
            self.get_job(&id)?.expect("inserted job"),
        ))
    }

    /// Insert a job row verbatim (recovery tests and imports).
    pub fn insert_job(&mut self, job: &JobInfo) -> Result<(), RegistryError> {
        self.conn.execute(
            "INSERT INTO jobs (job_id, project_id, kind, state, priority, options_json, created_at_ms,
               started_at_ms, finished_at_ms, result_json, error_json, attempt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                job.job_id,
                job.project_id,
                job.kind,
                job.state,
                job.priority,
                job.options.to_string(),
                job.created_at_ms,
                job.started_at_ms,
                job.finished_at_ms,
                job.result.as_ref().map(Value::to_string),
                job.error.as_ref().map(Value::to_string),
                job.attempt
            ],
        )?;
        Ok(())
    }

    pub fn get_job(&self, job_id: &str) -> Result<Option<JobInfo>, RegistryError> {
        Ok(self
            .conn
            .query_row(
                &format!("{JOB_SELECT} WHERE job_id = ?1"),
                [job_id],
                row_to_job,
            )
            .optional()?)
    }

    pub fn list_jobs(
        &self,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<JobInfo>, RegistryError> {
        let limit = limit.clamp(1, 500) as i64;
        let mut stmt = self.conn.prepare(&format!(
            "{JOB_SELECT} WHERE (?1 IS NULL OR project_id = ?1)
             ORDER BY created_at_ms DESC, job_id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![project_id, limit], row_to_job)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Queued jobs, highest priority first (ties: oldest first).
    pub fn queued_jobs(&self) -> Result<Vec<JobInfo>, RegistryError> {
        let mut stmt = self.conn.prepare(&format!(
            "{JOB_SELECT} WHERE state = 'queued' ORDER BY priority DESC, created_at_ms, job_id LIMIT 64"
        ))?;
        let rows = stmt.query_map([], row_to_job)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// queued -> running. False when the job is no longer queued (canceled).
    pub fn start_job(&mut self, job_id: &str) -> Result<bool, RegistryError> {
        Ok(self.conn.execute(
            "UPDATE jobs SET state = 'running', started_at_ms = ?2, attempt = attempt + 1
              WHERE job_id = ?1 AND state = 'queued'",
            params![job_id, now_ms()],
        )? > 0)
    }

    pub fn finish_job(
        &mut self,
        job_id: &str,
        ok: bool,
        result: Option<&Value>,
        error: Option<&Value>,
    ) -> Result<(), RegistryError> {
        self.conn.execute(
            "UPDATE jobs SET state = ?2, finished_at_ms = ?3, result_json = ?4, error_json = ?5
              WHERE job_id = ?1",
            params![
                job_id,
                if ok { "succeeded" } else { "failed" },
                now_ms(),
                result.map(Value::to_string),
                error.map(Value::to_string)
            ],
        )?;
        Ok(())
    }

    /// Cancel a queued job. Running jobs cannot be canceled.
    pub fn cancel_job(&mut self, job_id: &str) -> Result<bool, RegistryError> {
        Ok(self.conn.execute(
            "UPDATE jobs SET state = 'canceled', finished_at_ms = ?2
              WHERE job_id = ?1 AND state = 'queued'",
            params![job_id, now_ms()],
        )? > 0)
    }

    /// Startup recovery: jobs left `running` by a previous instance become
    /// `failed` (reason `interrupted`). Returns their distinct project ids.
    pub fn fail_interrupted_jobs(&mut self) -> Result<Vec<String>, RegistryError> {
        let tx = self.conn.transaction()?;
        let ids: Vec<String> = tx
            .prepare(
                "SELECT DISTINCT project_id FROM jobs WHERE state = 'running' ORDER BY project_id",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let error = json!({"code": "INTERRUPTED", "message": "interrupted"}).to_string();
        tx.execute(
            "UPDATE jobs SET state = 'failed', finished_at_ms = ?1, error_json = ?2
              WHERE state = 'running'",
            params![now_ms(), error],
        )?;
        tx.commit()?;
        Ok(ids)
    }
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
    fn single_flight_cancel_and_interrupt_recovery() {
        let (_t, mut r) = reg();
        let a = match r.admit_job("p1", "hygiene", 0, &json!({})).unwrap() {
            JobAdmission::Queued(j) => j,
            other => panic!("{other:?}"),
        };
        assert!(matches!(
            r.admit_job("p1", "hygiene", 5, &json!({})).unwrap(),
            JobAdmission::Coalesced(j) if j.job_id == a.job_id
        ));
        assert_eq!(r.get_job(&a.job_id).unwrap().unwrap().priority, 5);
        assert!(r.start_job(&a.job_id).unwrap());
        assert!(
            !r.cancel_job(&a.job_id).unwrap(),
            "running jobs are not cancelable"
        );
        assert_eq!(r.fail_interrupted_jobs().unwrap(), vec!["p1".to_string()]);
        let j = r.get_job(&a.job_id).unwrap().unwrap();
        assert_eq!(j.state, "failed");
        assert_eq!(j.error.unwrap()["code"], "INTERRUPTED");
        let b = match r.admit_job("p1", "hygiene", 0, &json!({})).unwrap() {
            JobAdmission::Queued(j) => j,
            other => panic!("{other:?}"),
        };
        assert!(r.cancel_job(&b.job_id).unwrap());
        assert!(!r.start_job(&b.job_id).unwrap());
    }

    #[test]
    fn queue_cap_and_schedule_revisions() {
        let (_t, mut r) = reg();
        for i in 0..MAX_QUEUED_JOBS {
            assert!(matches!(
                r.admit_job(&format!("p{i}"), "hygiene", 0, &json!({}))
                    .unwrap(),
                JobAdmission::Queued(_)
            ));
        }
        assert!(matches!(
            r.admit_job("px", "hygiene", 0, &json!({})).unwrap(),
            JobAdmission::Saturated
        ));
        let s = r.ensure_schedule("p1").unwrap();
        assert_eq!(s.cadence_ms, DEFAULT_CADENCE_MS);
        assert_eq!(s.options_revision, 1);
        let s2 = r
            .configure_schedule(
                "p1",
                Some(1000),
                Some(&json!({"verify": false})),
                None,
                Some(1),
            )
            .unwrap()
            .unwrap();
        assert_eq!(s2.options_revision, 2);
        assert!(s2.next_due_ms <= now_ms() + 1000);
        assert!(
            r.configure_schedule("p1", None, Some(&json!({})), None, Some(1))
                .unwrap()
                .is_err()
        );
        r.record_schedule_run("p1", "succeeded").unwrap();
        assert_eq!(
            r.get_schedule("p1")
                .unwrap()
                .unwrap()
                .last_outcome
                .as_deref(),
            Some("succeeded")
        );
    }

    #[test]
    fn v1_registry_migrates_to_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(crate::MIGRATION_1).unwrap();
            c.execute(
                "INSERT INTO registry_meta(key, value) VALUES ('schema_version', '1')",
                [],
            )
            .unwrap();
        }
        let mut r = Registry::open(&path).unwrap();
        r.ensure_schedule("p").unwrap();
    }
}
