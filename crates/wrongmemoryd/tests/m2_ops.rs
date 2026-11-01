//! M2 operations against the real daemon: durable maintenance scheduling,
//! interrupted-job recovery, single-flight jobs, the scoped change feed,
//! the open-store cap, and missing-store recovery (WM-205..208).

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongmemory_contracts::{ErrorCode, JobInfo, method};
use wrongmemory_ipc::{CallError, Client, DataRoot};
use wrongmemory_registry::Registry;

struct Daemon {
    child: Option<Child>,
}

impl Daemon {
    async fn start(root: &Path, extra: &[&str]) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_wrongmemoryd"))
            .arg("--data-root")
            .arg(root)
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn wrongmemoryd");
        let deadline = Instant::now() + Duration::from_secs(15);
        while Client::connect_host(&dr(root), "probe").await.is_err() {
            assert!(Instant::now() < deadline, "daemon did not become ready");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self { child: Some(child) }
    }

    async fn stop(mut self, root: &Path) {
        let mut c = host(root).await;
        call(&mut c, method::DAEMON_SHUTDOWN, None, json!({})).await;
        let mut child = self.child.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "daemon did not stop");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn dr(root: &Path) -> DataRoot {
    DataRoot::resolve(Some(root)).unwrap()
}

async fn host(root: &Path) -> Client {
    Client::connect_host(&dr(root), "m2-test").await.unwrap()
}

async fn call(c: &mut Client, m: &str, p: Option<&str>, params: Value) -> Value {
    c.call_raw(m, p, params, None)
        .await
        .unwrap_or_else(|e| panic!("{m} failed: {e}"))
}

async fn call_err(c: &mut Client, m: &str, p: Option<&str>, params: Value) -> ErrorCode {
    match c.call_raw(m, p, params, None).await {
        Err(CallError::Remote(e)) => e.code,
        other => panic!("{m}: expected a typed error, got {other:?}"),
    }
}

fn mkdir(p: PathBuf) -> PathBuf {
    std::fs::create_dir_all(&p).unwrap();
    p
}

async fn register(c: &mut Client, root: &Path) -> (String, PathBuf) {
    let v = call(
        c,
        method::PROJECT_REGISTER,
        None,
        json!({"root": root.display().to_string(), "storeKind": "managed", "write": true}),
    )
    .await;
    (
        v["project"]["projectId"].as_str().unwrap().to_string(),
        PathBuf::from(v["project"]["storePath"].as_str().unwrap()),
    )
}

async fn remember(c: &mut Client, pid: &str, text: &str, extra: Value) -> String {
    let mut params = json!({"text": text, "kind": "fact"});
    for (k, v) in extra.as_object().unwrap() {
        params[k] = v.clone();
    }
    call(c, method::MEMORY_REMEMBER, Some(pid), params).await["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn wait_for<F: FnMut(&Value) -> bool>(
    c: &mut Client,
    m: &str,
    pid: Option<&str>,
    params: Value,
    mut pred: F,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let v = call(c, m, pid, params.clone()).await;
        if pred(&v) {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting on {m}: {v}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_runs_due_jobs_and_recovers_interrupted_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let proj = mkdir(tmp.path().join("P"));
    let daemon = Daemon::start(&root, &["--scheduler-tick-ms", "200"]).await;
    let mut h = host(&root).await;
    let (pid, _) = register(&mut h, &proj).await;
    remember(&mut h, &pid, "Builds run on the release runner", json!({})).await;

    // Default schedule: 24h cadence, not due yet.
    let st = call(&mut h, method::MAINTENANCE_STATUS, Some(&pid), json!({})).await;
    assert_eq!(st["schedule"]["cadenceMs"], 86_400_000);
    assert!(st["schedule"]["lastRunMs"].is_null());

    // Options changes bump the revision; a stale revision is refused.
    let cfg = call(
        &mut h,
        method::MAINTENANCE_CONFIGURE,
        Some(&pid),
        json!({"options": {"nearDedup": false}, "expectedOptionsRevision": 1}),
    )
    .await;
    assert_eq!(cfg["schedule"]["optionsRevision"], 2);
    assert_eq!(
        call_err(
            &mut h,
            method::MAINTENANCE_CONFIGURE,
            Some(&pid),
            json!({"options": {}, "expectedOptionsRevision": 1})
        )
        .await,
        ErrorCode::RevisionConflict
    );
    assert_eq!(
        call_err(
            &mut h,
            method::MAINTENANCE_CONFIGURE,
            Some(&pid),
            json!({"cadenceMs": 5})
        )
        .await,
        ErrorCode::InvalidRequest
    );

    // A short cadence makes the schedule due; the scheduler runs it and
    // records the outcome durably.
    call(
        &mut h,
        method::MAINTENANCE_CONFIGURE,
        Some(&pid),
        json!({"cadenceMs": 1000}),
    )
    .await;
    let st = wait_for(
        &mut h,
        method::MAINTENANCE_STATUS,
        Some(&pid),
        json!({}),
        |v| v["schedule"]["lastOutcome"] == "succeeded",
    )
    .await;
    assert!(st["schedule"]["lastRunMs"].as_i64().unwrap() > 0);
    let jobs = call(&mut h, method::JOBS_LIST, Some(&pid), json!({})).await;
    let done: Vec<JobInfo> = serde_json::from_value(jobs["jobs"].clone()).unwrap();
    let ok = done.iter().find(|j| j.state == "succeeded").expect("job");
    assert_eq!(
        ok.options,
        json!({"nearDedup": false}),
        "stored options used"
    );
    assert!(ok.result.as_ref().unwrap()["examined"].as_u64().unwrap() >= 1);
    assert_eq!(ok.result.as_ref().unwrap()["dryRun"], false);

    // Back to a long cadence so nothing else runs, then stop.
    call(
        &mut h,
        method::MAINTENANCE_CONFIGURE,
        Some(&pid),
        json!({"cadenceMs": 86_400_000}),
    )
    .await;
    drop(h);
    daemon.stop(&root).await;

    // Simulate a crash mid-job: a job left `running` by a dead instance.
    {
        let mut reg = Registry::open(&dr(&root).registry_db()).unwrap();
        reg.insert_job(&JobInfo {
            job_id: "j_crashed".into(),
            project_id: pid.clone(),
            kind: "hygiene".into(),
            state: "running".into(),
            priority: 0,
            options: json!({}),
            created_at_ms: 1,
            started_at_ms: Some(2),
            finished_at_ms: None,
            result: None,
            error: None,
            attempt: 1,
        })
        .unwrap();
    }
    let daemon = Daemon::start(&root, &["--scheduler-tick-ms", "200"]).await;
    let mut h = host(&root).await;
    let crashed = call(
        &mut h,
        method::JOBS_GET,
        None,
        json!({"jobId": "j_crashed"}),
    )
    .await;
    assert_eq!(crashed["state"], "failed");
    assert_eq!(crashed["error"]["code"], "INTERRUPTED");
    // Rescheduled: a fresh job for the project runs to completion.
    wait_for(&mut h, method::JOBS_LIST, Some(&pid), json!({}), |v| {
        v["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["jobId"] != "j_crashed" && j["state"] == "succeeded")
    })
    .await;
    assert_eq!(
        call_err(&mut h, method::JOBS_GET, None, json!({"jobId": "j_nope"})).await,
        ErrorCode::NotFound
    );
    drop(h);
    daemon.stop(&root).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_runs_are_single_flight_and_cancelable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let proj = mkdir(tmp.path().join("P"));
    // Long tick: nothing executes during the test window.
    let daemon = Daemon::start(&root, &["--scheduler-tick-ms", "60000"]).await;
    let mut h = host(&root).await;
    let (pid, _) = register(&mut h, &proj).await;

    let first = call(&mut h, method::MAINTENANCE_RUN, Some(&pid), json!({})).await;
    assert_eq!(first["coalesced"], false);
    assert_eq!(first["job"]["state"], "queued");
    let second = call(
        &mut h,
        method::MAINTENANCE_RUN,
        Some(&pid),
        json!({"options": {"verify": false}}),
    )
    .await;
    assert_eq!(second["coalesced"], true);
    assert_eq!(second["job"]["jobId"], first["job"]["jobId"]);
    let queued = call(&mut h, method::JOBS_LIST, Some(&pid), json!({})).await;
    assert_eq!(queued["jobs"].as_array().unwrap().len(), 1);

    let id = first["job"]["jobId"].as_str().unwrap();
    let c = call(&mut h, method::JOBS_CANCEL, None, json!({"jobId": id})).await;
    assert_eq!(c["job"]["state"], "canceled");
    assert_eq!(
        call_err(&mut h, method::JOBS_CANCEL, None, json!({"jobId": id})).await,
        ErrorCode::InvalidRequest
    );
    // After cancellation a new run is admitted again.
    let third = call(&mut h, method::MAINTENANCE_RUN, Some(&pid), json!({})).await;
    assert_eq!(third["coalesced"], false);
    assert_ne!(third["job"]["jobId"], first["job"]["jobId"]);
    assert_eq!(
        call_err(
            &mut h,
            method::MAINTENANCE_RUN,
            Some(&pid),
            json!({"options": "bad"})
        )
        .await,
        ErrorCode::InvalidRequest
    );
    drop(h);
    daemon.stop(&root).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn change_feed_orders_resyncs_and_keeps_sessions_private() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let proj = mkdir(tmp.path().join("P"));
    let daemon = Daemon::start(&root, &["--scheduler-tick-ms", "60000"]).await;
    let mut a = host(&root).await;
    let mut b = host(&root).await;
    let (pid, _) = register(&mut a, &proj).await;
    call(
        &mut a,
        method::SESSION_ACQUIRE,
        None,
        json!({"hostSessionId": "sess-a"}),
    )
    .await;
    call(
        &mut b,
        method::SESSION_ACQUIRE,
        None,
        json!({"hostSessionId": "sess-b"}),
    )
    .await;

    let start = call(
        &mut a,
        method::EVENTS_SINCE,
        Some(&pid),
        json!({"generation": 0}),
    )
    .await;
    assert_eq!(start["resync"], false);
    assert_eq!(start["generation"], 0);
    let instance = start["daemonInstanceId"].as_str().unwrap().to_string();

    let shared = remember(&mut a, &pid, "Shared fact about the cache layer", json!({})).await;
    let private = remember(
        &mut a,
        &pid,
        "Private scratch note for session A only",
        json!({"scope": "session"}),
    )
    .await;
    call(
        &mut a,
        method::MEMORY_UPDATE,
        Some(&pid),
        json!({"id": shared, "confidence": 0.9}),
    )
    .await;

    // Owner sees all three, in commit order, without memory text.
    let ev = call(
        &mut a,
        method::EVENTS_SINCE,
        Some(&pid),
        json!({"generation": 0}),
    )
    .await;
    let list = ev["events"].as_array().unwrap();
    assert_eq!(ev["generation"], 3);
    assert_eq!(list.len(), 3, "{ev}");
    let gens: Vec<u64> = list
        .iter()
        .map(|e| e["generation"].as_u64().unwrap())
        .collect();
    assert_eq!(gens, vec![1, 2, 3]);
    assert_eq!(list[1]["memoryId"], private.as_str());
    assert_eq!(list[2]["change"], "memory.update");
    assert!(list[2]["revision"].as_i64().unwrap() >= 2);
    assert!(!ev.to_string().contains("scratch note"));

    // Another session never learns about A's session memory.
    let ev_b = call(
        &mut b,
        method::EVENTS_SINCE,
        Some(&pid),
        json!({"generation": 0}),
    )
    .await;
    let ids: Vec<&str> = ev_b["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["memoryId"].as_str())
        .collect();
    assert_eq!(ids, vec![shared.as_str(), shared.as_str()]);
    assert!(!ev_b.to_string().contains(&private));

    // Incremental read.
    let inc = call(
        &mut b,
        method::EVENTS_SINCE,
        Some(&pid),
        json!({"generation": 2}),
    )
    .await;
    assert_eq!(inc["events"].as_array().unwrap().len(), 1);
    // Replayed mutations publish nothing new.
    let key = "idem-feed-1";
    for _ in 0..2 {
        a.call_raw(
            method::MEMORY_REMEMBER,
            Some(&pid),
            json!({"text": "Idempotent fact about retries", "kind": "fact"}),
            Some(key),
        )
        .await
        .unwrap();
    }
    let ev = call(
        &mut a,
        method::EVENTS_SINCE,
        Some(&pid),
        json!({"generation": 3}),
    )
    .await;
    assert_eq!(ev["events"].as_array().unwrap().len(), 1);

    // Future generation or another daemon instance: resync.
    let r = call(
        &mut a,
        method::EVENTS_SINCE,
        Some(&pid),
        json!({"generation": 999}),
    )
    .await;
    assert_eq!(r["resync"], true);
    let r = call(
        &mut a,
        method::EVENTS_SINCE,
        Some(&pid),
        json!({"generation": 1, "daemonInstanceId": "d_other"}),
    )
    .await;
    assert_eq!(r["resync"], true);
    assert_eq!(r["daemonInstanceId"], instance.as_str());
    assert_eq!(
        call_err(
            &mut a,
            method::EVENTS_SINCE,
            Some("p_unknown"),
            json!({"generation": 0})
        )
        .await,
        ErrorCode::ProjectNotFound
    );
    drop((a, b));
    daemon.stop(&root).await;

    // After a restart the old instance's generations are meaningless.
    let daemon = Daemon::start(&root, &["--scheduler-tick-ms", "60000"]).await;
    let mut a = host(&root).await;
    let r = call(
        &mut a,
        method::EVENTS_SINCE,
        Some(&pid),
        json!({"generation": 4, "daemonInstanceId": instance}),
    )
    .await;
    assert_eq!(r["resync"], true);
    assert_ne!(r["daemonInstanceId"], instance.as_str());
    drop(a);
    daemon.stop(&root).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_cap_evicts_idle_handles_and_missing_stores_recover() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let p1 = mkdir(tmp.path().join("P1"));
    let p2 = mkdir(tmp.path().join("P2"));
    let daemon = Daemon::start(
        &root,
        &["--scheduler-tick-ms", "60000", "--max-open-stores", "1"],
    )
    .await;
    let mut h = host(&root).await;
    let (a, a_db) = register(&mut h, &p1).await;
    let (b, _) = register(&mut h, &p2).await;
    let ma = remember(&mut h, &a, "Project one uses tabs", json!({})).await;
    let health = call(&mut h, method::DAEMON_HEALTH, None, json!({})).await;
    assert_eq!(health["openStores"], 1);
    remember(&mut h, &b, "Project two uses spaces", json!({})).await;
    let health = call(&mut h, method::DAEMON_HEALTH, None, json!({})).await;
    assert_eq!(health["openStores"], 1, "cap holds; A was evicted");
    // Reopening A works (and evicts B); data is intact.
    let got = call(&mut h, method::MEMORY_GET, Some(&a), json!({"id": ma})).await;
    assert_eq!(got["id"], ma.as_str());

    // Evict A again, then take its store away: typed error, no recreation.
    call(&mut h, method::MEMORY_STATS, Some(&b), json!({})).await;
    let moved = a_db.with_extension("db.moved");
    std::fs::rename(&a_db, &moved).unwrap();
    assert_eq!(
        call_err(&mut h, method::MEMORY_GET, Some(&a), json!({"id": ma})).await,
        ErrorCode::StoreUnavailable
    );
    assert!(!a_db.exists(), "an absent store is never recreated");
    // Other projects keep working.
    call(&mut h, method::MEMORY_STATS, Some(&b), json!({})).await;

    // Restore the file: the next request reopens it.
    std::fs::rename(&moved, &a_db).unwrap();
    let got = call(&mut h, method::MEMORY_GET, Some(&a), json!({"id": ma})).await;
    assert_eq!(got["id"], ma.as_str());
    drop(h);
    daemon.stop(&root).await;
}

/// Concurrent requests for a project whose store is not open (and the
/// eviction churn a small cap causes) must never race on store ownership:
/// found by the soak run, where ~9% of requests failed with OWNER_CONFLICT
/// against the daemon's own lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_opens_under_eviction_never_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let daemon = Daemon::start(
        &root,
        &["--scheduler-tick-ms", "60000", "--max-open-stores", "2"],
    )
    .await;
    let mut h = host(&root).await;
    let mut projects = Vec::new();
    for i in 0..6 {
        let (p, _) = register(&mut h, &mkdir(tmp.path().join(format!("P{i}")))).await;
        remember(
            &mut h,
            &p,
            &format!("Project {i} keeps a changelog"),
            json!({}),
        )
        .await;
        projects.push(p);
    }
    let mut tasks = Vec::new();
    for w in 0..8usize {
        let root = root.clone();
        let projects = projects.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = host(&root).await;
            let mut failures = Vec::new();
            for k in 0..150usize {
                let p = &projects[(w * 7 + k * 5) % projects.len()];
                if let Err(e) = c
                    .call_raw(
                        method::MEMORY_SEARCH,
                        Some(p),
                        json!({"query": "changelog"}),
                        None,
                    )
                    .await
                {
                    failures.push(e.to_string());
                }
            }
            failures
        }));
    }
    let mut failures = Vec::new();
    for t in tasks {
        failures.extend(t.await.unwrap());
    }
    assert!(
        failures.is_empty(),
        "{} failed, first: {:?}",
        failures.len(),
        failures.first()
    );
    drop(h);
    daemon.stop(&root).await;
}

/// WM-601: `project.export` hands a managed project back to in-process
/// SAGE. The copy lands in `<root>/.wrongstack/memories/sage.db`, is a
/// compatible SAGE store with the same records, and the managed registration
/// is detached so the two stores cannot diverge. An occupied legacy location
/// is refused without detaching, adopted stores are not exported, and the
/// operation is host-only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_store_exports_to_the_legacy_location_and_detaches() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let d = Daemon::start(&home, &[]).await;
    let mut c = host(&home).await;

    // Occupied legacy location: refused, nothing detached or written.
    let busy = mkdir(tmp.path().join("busy"));
    let (busy_id, _) = register(&mut c, &busy).await;
    remember(&mut c, &busy_id, "Busy project keeps serving", json!({})).await;
    let busy_legacy = mkdir(busy.join(".wrongstack").join("memories"));
    std::fs::write(busy_legacy.join("sage.db-wal"), b"").unwrap();
    assert_eq!(
        call_err(
            &mut c,
            method::PROJECT_EXPORT,
            None,
            json!({"projectId": busy_id})
        )
        .await,
        ErrorCode::StoreIdentityConflict
    );
    assert!(!busy_legacy.join("sage.db").exists());
    let still = call(
        &mut c,
        method::MEMORY_SEARCH,
        Some(&busy_id),
        json!({"query": "busy project"}),
    )
    .await;
    assert_eq!(still["hits"].as_array().unwrap().len(), 1);

    // Admin only: a curator grant cannot export.
    let g = call(
        &mut c,
        method::GRANT_CREATE,
        None,
        json!({"label": "cur", "profile": "curator", "projects": [busy_id]}),
    )
    .await;
    let grant_id = g["grant"]["grantId"].as_str().unwrap().to_string();
    let mut cur = Client::connect_grant(&dr(&home), &grant_id, "cur")
        .await
        .unwrap();
    assert_eq!(
        call_err(
            &mut cur,
            method::PROJECT_EXPORT,
            None,
            json!({"projectId": busy_id})
        )
        .await,
        ErrorCode::Forbidden
    );

    // The happy path.
    let proj = mkdir(tmp.path().join("proj"));
    let (pid, managed_store) = register(&mut c, &proj).await;
    remember(&mut c, &pid, "Exported fact about release tags", json!({})).await;
    remember(&mut c, &pid, "Second exported fact about lint", json!({})).await;
    let v = call(
        &mut c,
        method::PROJECT_EXPORT,
        None,
        json!({"projectId": pid}),
    )
    .await;
    let legacy = proj.join(".wrongstack").join("memories").join("sage.db");
    assert_eq!(v["detached"], true, "{v}");
    assert_eq!(v["backup"]["integrity"], "ok", "{v}");
    assert_eq!(v["backup"]["memoryRows"], 2, "{v}");
    assert!(legacy.exists());
    assert!(managed_store.exists(), "the managed store is kept");
    let insp = wrongmemory_storage::inspect::inspect(&legacy);
    assert_eq!(
        insp.verdict,
        wrongmemory_storage::inspect::Verdict::Compatible,
        "{}",
        insp.detail
    );
    assert_eq!(insp.memory_rows, Some(2));

    // The managed registration is gone: hidden like any unknown project.
    assert_eq!(
        call_err(
            &mut c,
            method::MEMORY_SEARCH,
            Some(&pid),
            json!({"query": "release tags"})
        )
        .await,
        ErrorCode::ProjectNotFound
    );
    assert_eq!(
        call_err(
            &mut c,
            method::PROJECT_EXPORT,
            None,
            json!({"projectId": pid})
        )
        .await,
        ErrorCode::ProjectNotFound
    );

    // The exported copy is an ordinary adoptable SAGE store, and an adopted
    // store is never exported again.
    let adopted = call(
        &mut c,
        method::PROJECT_REGISTER,
        None,
        json!({"root": proj.display().to_string(), "storeKind": "adopted", "write": false}),
    )
    .await;
    let aid = adopted["project"]["projectId"]
        .as_str()
        .unwrap()
        .to_string();
    let found = call(
        &mut c,
        method::MEMORY_SEARCH,
        Some(&aid),
        json!({"query": "release tags"}),
    )
    .await;
    assert_eq!(found["hits"].as_array().unwrap().len(), 1, "{found}");
    assert_eq!(
        call_err(
            &mut c,
            method::PROJECT_EXPORT,
            None,
            json!({"projectId": aid})
        )
        .await,
        ErrorCode::InvalidRequest
    );
    drop(cur);
    drop(c);
    d.stop(&home).await;
}
