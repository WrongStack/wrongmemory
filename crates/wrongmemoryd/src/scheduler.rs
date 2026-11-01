//! Durable per-project maintenance (WM-205/206).
//!
//! Schedules and jobs live in the registry, so they survive restarts. The
//! scheduler loop ticks on a fixed interval and:
//!
//! * gives every writable project a schedule (default cadence 24h);
//! * admits a hygiene job for each due schedule (single-flight per project,
//!   queue cap with coalescing, see `Registry::admit_job`);
//! * runs at most one heavy job at a time, and only when no interactive
//!   request is waiting for or holding that project's lane, so maintenance
//!   never starves interactive work.
//!
//! On startup, jobs left `running` by a previous instance are recorded as
//! failed (`interrupted`) and requeued; overdue schedules are staggered one
//! tick apart so a restart does not stampede every store at once. A job
//! running during shutdown is counted as in-flight work: the drain waits for
//! it and its outcome is recorded before stores close.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};
use wrongmemory_contracts::{Admission, ErrorBody, ErrorCode, JobInfo};
use wrongmemory_registry::jobs::JobAdmission;
use wrongmemory_registry::{now_iso, now_ms};
use wrongmemory_storage::MutationCtx;
use wrongmemory_storage::hygiene::HygieneOptions;

use crate::events;
use crate::projects::invalidates_handle;
use crate::server::Shared;
use crate::{info, warn};

pub const KIND_HYGIENE: &str = "hygiene";
pub const PRIORITY_SCHEDULED: i64 = 0;
pub const PRIORITY_MANUAL: i64 = 10;
pub const DEFAULT_TICK: Duration = Duration::from_secs(5);

pub struct Scheduler {
    pub tick: Duration,
    /// Job currently executing (global cap: one heavy job).
    running: Mutex<Option<String>>,
}

impl Scheduler {
    pub fn new(tick: Duration) -> Self {
        Self {
            tick: tick.max(Duration::from_millis(50)),
            running: Mutex::new(None),
        }
    }

    pub fn running_job(&self) -> Option<String> {
        self.running.lock().ok().and_then(|r| r.clone())
    }
}

fn tick_ms(shared: &Shared) -> i64 {
    shared.scheduler.tick.as_millis() as i64
}

/// Startup recovery: fail interrupted jobs, requeue them, stagger catch-up.
pub fn recover(shared: &Shared) -> Result<(), ErrorBody> {
    let interrupted = shared.reg(|r| r.fail_interrupted_jobs())?;
    for pid in &interrupted {
        let options = shared
            .reg(|r| r.get_schedule(pid))?
            .map(|s| s.options)
            .unwrap_or_else(|| json!({}));
        let admitted =
            shared.reg(|r| r.admit_job(pid, KIND_HYGIENE, PRIORITY_SCHEDULED, &options))?;
        info!("job_interrupted_requeued", {
            "projectId": pid,
            "requeued": matches!(admitted, JobAdmission::Queued(_) | JobAdmission::Coalesced(_)),
        });
    }
    let now = now_ms();
    let step = tick_ms(shared);
    let overdue = shared.reg(|r| r.due_schedules(now))?;
    for (i, s) in overdue.iter().enumerate() {
        let at = now + step * i as i64;
        shared.reg(|r| r.set_next_due(&s.project_id, at))?;
    }
    if !overdue.is_empty() {
        info!("maintenance_catch_up", {"projects": overdue.len(), "staggerMs": step});
    }
    Ok(())
}

/// Scheduler loop; exits when the daemon starts draining.
pub async fn run(shared: Arc<Shared>) {
    let mut shutdown = shared.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(shared.scheduler.tick) => {}
            _ = shutdown.changed() => break,
        }
        if shared.draining.load(Ordering::SeqCst) {
            break;
        }
        let me = shared.clone();
        let picked = tokio::task::spawn_blocking(move || admit_and_pick(&me)).await;
        let job = match picked {
            Ok(Ok(Some(job))) => job,
            Ok(Ok(None)) => continue,
            Ok(Err(e)) => {
                warn!("scheduler_tick_failed", {"code": e.code.as_str(), "error": e.message});
                continue;
            }
            Err(e) => {
                warn!("scheduler_tick_failed", {"error": e.to_string()});
                continue;
            }
        };
        execute(&shared, job).await;
    }
}

/// One tick of bookkeeping: schedules, due admission, and selection of the
/// next runnable job (marked running). Blocking.
fn admit_and_pick(shared: &Shared) -> Result<Option<JobInfo>, ErrorBody> {
    let now = now_ms();
    for p in shared.reg(|r| r.list_projects())? {
        if p.admission == Admission::ReadWrite {
            shared.reg(|r| r.ensure_schedule(&p.project_id).map(|_| ()))?;
        }
    }
    for s in shared.reg(|r| r.due_schedules(now))? {
        match shared
            .reg(|r| r.admit_job(&s.project_id, KIND_HYGIENE, PRIORITY_SCHEDULED, &s.options))?
        {
            JobAdmission::Saturated => break,
            _ => {
                // Not due again until this run records its outcome.
                shared.reg(|r| r.set_next_due(&s.project_id, now + s.cadence_ms))?;
            }
        }
    }
    if shared.scheduler.running_job().is_some() || shared.draining.load(Ordering::SeqCst) {
        return Ok(None);
    }
    for job in shared.reg(|r| r.queued_jobs())? {
        // Yield to interactive requests on this project.
        if shared
            .stores
            .peek(&job.project_id)
            .is_some_and(|h| h.waiting() > 0)
        {
            continue;
        }
        if shared.reg(|r| r.start_job(&job.job_id))? {
            if let Ok(mut r) = shared.scheduler.running.lock() {
                *r = Some(job.job_id.clone());
            }
            return shared.reg(|r| r.get_job(&job.job_id));
        }
    }
    Ok(None)
}

/// Run a job that was marked running; record its outcome.
async fn execute(shared: &Arc<Shared>, job: JobInfo) {
    shared.inflight.fetch_add(1, Ordering::SeqCst);
    let me = shared.clone();
    let j = job.clone();
    let outcome = tokio::task::spawn_blocking(move || run_hygiene(&me, &j))
        .await
        .unwrap_or_else(|e| {
            Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("job worker failed: {e}"),
            ))
        });
    let me = shared.clone();
    let pid = job.project_id.clone();
    let ok = outcome.is_ok();
    let (result, error) = match &outcome {
        Ok(v) => (Some(v.clone()), None),
        Err(e) => (
            None,
            Some(json!({"code": e.code.as_str(), "message": e.message})),
        ),
    };
    let invalidate = outcome.as_ref().err().is_some_and(invalidates_handle);
    let job_id = job.job_id.clone();
    let recorded = tokio::task::spawn_blocking(move || {
        if invalidate {
            me.stores
                .invalidate(&pid, &me.registry, &me.instance_id, "store_error");
        }
        me.reg(|r| r.finish_job(&job_id, ok, result.as_ref(), error.as_ref()))?;
        me.reg(|r| r.record_schedule_run(&pid, if ok { "succeeded" } else { "failed" }))
    })
    .await;
    if ok {
        shared
            .events
            .publish(&job.project_id, vec![events::maintenance_entry()]);
    }
    if let Ok(mut r) = shared.scheduler.running.lock() {
        *r = None;
    }
    shared.inflight.fetch_sub(1, Ordering::SeqCst);
    match (&outcome, recorded) {
        (_, Ok(Err(e))) => warn!("job_record_failed", {"jobId": job.job_id, "error": e.message}),
        (_, Err(e)) => warn!("job_record_failed", {"jobId": job.job_id, "error": e.to_string()}),
        (Ok(_), _) => info!("job_succeeded", {"jobId": job.job_id, "projectId": job.project_id}),
        (Err(e), _) => {
            warn!("job_failed", {"jobId": job.job_id, "projectId": job.project_id, "code": e.code.as_str()})
        }
    }
}

fn run_hygiene(shared: &Shared, job: &JobInfo) -> Result<Value, ErrorBody> {
    if job.kind != KIND_HYGIENE {
        return Err(ErrorBody::new(
            ErrorCode::InvalidRequest,
            format!("unknown job kind {}", job.kind),
        ));
    }
    let mut opts: HygieneOptions = serde_json::from_value(job.options.clone())
        .map_err(|e| ErrorBody::new(ErrorCode::InvalidRequest, format!("invalid options: {e}")))?;
    opts.dry_run = false;
    let h = shared
        .stores
        .handle(&shared.registry, &shared.instance_id, &job.project_id)?;
    if !h.writable() {
        return Err(ErrorBody::new(
            ErrorCode::ReadOnly,
            "maintenance requires a writable store",
        ));
    }
    let ctx = MutationCtx {
        principal_id: "daemon:maintenance".into(),
        client_label: "scheduler".into(),
        session_id: None,
        idempotency_key: None,
        now_iso: now_iso(),
    };
    // Maintenance takes the lane uncounted: it must not look like an
    // interactive waiter to itself or to other maintenance decisions.
    let mut store = h
        .store
        .lock()
        .map_err(|_| ErrorBody::new(ErrorCode::Internal, "store lock poisoned"))?;
    // An export may have retired this handle while the job waited for the
    // lane: the managed copy must not change after its snapshot.
    if h.is_retired() {
        return Err(ErrorBody::new(
            ErrorCode::ProjectNotFound,
            "project not found",
        ));
    }
    let report = store.hygiene(&opts, &ctx)?;
    self_heal(&mut store, &opts, &ctx, &job.project_id);
    serde_json::to_value(report).map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))
}

/// Repairs a scheduled pass applies on its own (`SELF_HEAL_FIXES` in the
/// baseline project server): structural only, never `hygiene` again.
const SELF_HEAL_FIXES: [&str; 4] = [
    "graph-hubs",
    "dangling-edges",
    "audience-runtime-mode",
    "fts-rebuild",
];

/// `selfHealAfterMaintenance`: run the doctor, apply the structural repairs
/// its checks ask for, and log what remains. Failures are logged, never
/// returned: they must not fail the scheduled hygiene run.
fn self_heal(
    store: &mut wrongmemory_storage::SageStore,
    opts: &HygieneOptions,
    ctx: &MutationCtx,
    project_id: &str,
) {
    let mut run = || -> Result<Value, ErrorBody> {
        let report = store.doctor(&[], opts, ctx)?;
        let mut fix: Vec<String> = Vec::new();
        for c in report["checks"].as_array().into_iter().flatten() {
            if let Some(f) = c["fix"].as_str()
                && c["severity"] != "ok"
                && SELF_HEAL_FIXES.contains(&f)
                && !fix.iter().any(|x| x == f)
            {
                fix.push(f.to_string());
            }
        }
        if fix.is_empty() {
            Ok(report)
        } else {
            store.doctor(&fix, opts, ctx)
        }
    };
    match run() {
        Ok(healed) => {
            for a in healed["fixesApplied"].as_array().into_iter().flatten() {
                info!("doctor_self_healed", {"projectId": project_id, "fix": a["fix"], "detail": a["detail"]});
            }
            for c in healed["checks"].as_array().into_iter().flatten() {
                if c["severity"] == "warn" || c["severity"] == "error" {
                    warn!("doctor_check", {"projectId": project_id, "severity": c["severity"], "check": c["id"], "summary": c["summary"]});
                }
            }
        }
        Err(e) => {
            warn!("doctor_self_heal_failed", {"projectId": project_id, "code": e.code.as_str()})
        }
    }
}

/// Validate hygiene options JSON (rejects unknown shapes early).
pub fn validate_options(options: &Value) -> Result<(), ErrorBody> {
    if !options.is_object() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidRequest,
            "options must be an object",
        ));
    }
    serde_json::from_value::<HygieneOptions>(options.clone())
        .map(|_| ())
        .map_err(|e| ErrorBody::new(ErrorCode::InvalidRequest, format!("invalid options: {e}")))
}
