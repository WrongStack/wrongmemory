//! WM-602 fault injection against the real daemon process (debug builds
//! only; `WRONGMEMORY_FAULT` is inert in release):
//!
//! * crash after commit, before the reply: an idempotent retry after restart
//!   returns the original outcome and never duplicates the write;
//! * injected I/O failure before COMMIT: typed error, nothing persisted (no
//!   memory, no receipt), and the very next write on the same store works;
//! * candidate accept crash: the acceptance is durable exactly once;
//! * a daemon killed while idle leaves stale discovery and an owner lock that
//!   a fresh daemon recovers from.

#![cfg(all(windows, debug_assertions))]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongmemory_contracts::method;
use wrongmemory_ipc::{CallError, Client, DataRoot};

struct Daemon(Option<Child>);

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Daemon {
    async fn start(root: &Path, fault: Option<&str>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wrongmemoryd"));
        cmd.arg("--data-root")
            .arg(root)
            .env_remove("WRONGMEMORY_FAULT")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(f) = fault {
            cmd.env("WRONGMEMORY_FAULT", f);
        }
        let child = cmd.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Client::connect_host(&dr(root), "probe").await.is_err() {
            assert!(Instant::now() < deadline, "daemon not ready");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self(Some(child))
    }

    /// Wait for the process to die on its own (injected abort).
    async fn expect_crash(&mut self) {
        let child = self.0.as_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(!status.success(), "fault must abort, got {status}");
                break;
            }
            assert!(Instant::now() < deadline, "daemon did not crash");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.0 = None;
    }

    async fn stop(mut self, root: &Path) {
        let mut c = Client::connect_host(&dr(root), "stop").await.unwrap();
        let _ = c
            .call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
            .await;
        let mut child = self.0.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "daemon did not stop");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn dr(root: &Path) -> DataRoot {
    DataRoot::resolve(Some(root)).unwrap()
}

async fn host(root: &Path) -> Client {
    Client::connect_host(&dr(root), "host").await.unwrap()
}

async fn call(c: &mut Client, m: &str, p: &str, params: Value) -> Value {
    c.call_raw(m, Some(p), params, None)
        .await
        .unwrap_or_else(|e| panic!("{m}: {e}"))
}

async fn register(c: &mut Client, proj: &Path) -> String {
    std::fs::create_dir_all(proj).unwrap();
    let v = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"root": proj.display().to_string(), "storeKind": "managed", "write": true}),
            None,
        )
        .await
        .unwrap();
    v["project"]["projectId"].as_str().unwrap().to_string()
}

/// Every non-deleted memory row whose text contains `needle`, as the host
/// (no session) sees it.
async fn rows_with(c: &mut Client, p: &str, needle: &str) -> Vec<String> {
    let page = call(c, method::MEMORY_LIST_PAGE, p, json!({"limit": 200})).await;
    page["items"]
        .as_array()
        .or_else(|| page["memories"].as_array())
        .expect("listPage items")
        .iter()
        .filter(|m| m["text"].as_str().unwrap_or("").contains(needle))
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_commit_before_reply_replays_idempotent_remember() {
    for round in 0..5 {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        let mut d = Daemon::start(&root, Some("after_commit_before_reply:memory.remember")).await;
        let mut h = host(&root).await;
        let p = register(&mut h, &proj).await;
        let params = json!({"text": format!("Crash-safe deploy fact round {round}")});
        let first = h
            .call_raw(
                method::MEMORY_REMEMBER,
                Some(&p),
                params.clone(),
                Some("k-crash"),
            )
            .await;
        assert!(
            matches!(first, Err(CallError::Transport(_) | CallError::Protocol(_))),
            "client must observe a transport failure, got {first:?}"
        );
        d.expect_crash().await;

        // Stale discovery + abandoned owner lock: a fresh daemon recovers.
        let d = Daemon::start(&root, None).await;
        let mut h = host(&root).await;
        let retry = h
            .call_raw(
                method::MEMORY_REMEMBER,
                Some(&p),
                params.clone(),
                Some("k-crash"),
            )
            .await
            .unwrap();
        assert_eq!(retry["replayed"], true, "round {round}: receipt replay");
        let ids = rows_with(&mut h, &p, "Crash-safe deploy fact").await;
        assert_eq!(ids, vec![retry["id"].as_str().unwrap().to_string()]);
        // A second retry is still the same single outcome.
        let again = h
            .call_raw(method::MEMORY_REMEMBER, Some(&p), params, Some("k-crash"))
            .await
            .unwrap();
        assert_eq!(again["id"], retry["id"]);
        assert_eq!(
            rows_with(&mut h, &p, "Crash-safe deploy fact").await.len(),
            1
        );
        drop(h);
        d.stop(&root).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_failure_before_commit_rolls_back_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let d = Daemon::start(&root, Some("before_commit")).await;
    let mut h = host(&root).await;
    let p = register(&mut h, &tmp.path().join("proj")).await;
    let params = json!({"text": "Rollback-safe cache invalidation fact"});
    let failed = h
        .call_raw(
            method::MEMORY_REMEMBER,
            Some(&p),
            params.clone(),
            Some("k-io"),
        )
        .await;
    match failed {
        Err(CallError::Remote(e)) => assert!(e.message.contains("before_commit"), "{e:?}"),
        other => panic!("expected a typed error, got {other:?}"),
    }
    // Nothing persisted: no memory row …
    assert!(rows_with(&mut h, &p, "Rollback-safe").await.is_empty());
    // … and no receipt: the same key now executes fresh (not a replay).
    let ok = h
        .call_raw(
            method::MEMORY_REMEMBER,
            Some(&p),
            params.clone(),
            Some("k-io"),
        )
        .await
        .unwrap();
    assert_eq!(ok["replayed"], false);
    let ids = rows_with(&mut h, &p, "Rollback-safe").await;
    assert_eq!(ids, vec![ok["id"].as_str().unwrap().to_string()]);
    // The store keeps working for unrelated writes.
    call(
        &mut h,
        method::MEMORY_REMEMBER,
        &p,
        json!({"text": "A later independent write succeeds"}),
    )
    .await;
    drop(h);
    d.stop(&root).await;

    // Durable across restart.
    let d = Daemon::start(&root, None).await;
    let mut h = host(&root).await;
    assert_eq!(rows_with(&mut h, &p, "Rollback-safe").await.len(), 1);
    let replay = h
        .call_raw(method::MEMORY_REMEMBER, Some(&p), params, Some("k-io"))
        .await
        .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["id"], ok["id"]);
    drop(h);
    d.stop(&root).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_during_candidate_accept_is_accepted_exactly_once() {
    for round in 0..5 {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        let mut d = Daemon::start(&root, Some("after_commit_before_reply:candidate.accept")).await;
        let mut h = host(&root).await;
        let p = register(&mut h, &tmp.path().join("proj")).await;
        let text = format!("Accepted-once queue retry policy round {round}");
        let cid = call(&mut h, method::CANDIDATE_PROPOSE, &p, json!({"text": text})).await["id"]
            .as_str()
            .unwrap()
            .to_string();
        let first = h
            .call_raw(method::CANDIDATE_ACCEPT, Some(&p), json!({"id": cid}), None)
            .await;
        assert!(
            matches!(first, Err(CallError::Transport(_) | CallError::Protocol(_))),
            "got {first:?}"
        );
        d.expect_crash().await;

        let d = Daemon::start(&root, None).await;
        let mut h = host(&root).await;
        let accepted = call(
            &mut h,
            method::CANDIDATE_LIST,
            &p,
            json!({"status": "accepted"}),
        )
        .await;
        let cand = accepted["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == cid.as_str())
            .unwrap_or_else(|| panic!("round {round}: candidate not accepted: {accepted}"))
            .clone();
        let mid = cand["acceptedMemoryId"]
            .as_str()
            .or_else(|| cand["memoryId"].as_str())
            .or_else(|| cand["resolution"]["memoryId"].as_str())
            .unwrap_or_else(|| panic!("no memory id on {cand}"))
            .to_string();
        assert_eq!(
            rows_with(&mut h, &p, "Accepted-once queue retry").await,
            vec![mid.clone()]
        );
        let again = call(&mut h, method::CANDIDATE_ACCEPT, &p, json!({"id": cid})).await;
        assert_eq!(again["alreadyResolved"], true);
        assert_eq!(
            rows_with(&mut h, &p, "Accepted-once queue retry")
                .await
                .len(),
            1
        );
        drop(h);
        d.stop(&root).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_kill_leaves_recoverable_discovery_and_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    let mut d = Daemon::start(&root, None).await;
    let mut h = host(&root).await;
    let p = register(&mut h, &proj).await;
    call(
        &mut h,
        method::MEMORY_REMEMBER,
        &p,
        json!({"text": "Survives a hard kill"}),
    )
    .await;
    drop(h);
    for _ in 0..5 {
        let mut child = d.0.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            dr(&root).endpoint_file().exists(),
            "a hard kill cannot retract discovery"
        );
        // The stale endpoint refuses: nobody is listening.
        assert!(Client::connect_host(&dr(&root), "stale").await.is_err());
        d = Daemon::start(&root, None).await;
        let mut h = host(&root).await;
        // The store's owner lock was released by the OS; writes still work.
        assert_eq!(rows_with(&mut h, &p, "Survives a hard kill").await.len(), 1);
        call(
            &mut h,
            method::MEMORY_RECORD_USAGE,
            &p,
            json!({"ids": [], "kind": "use"}),
        )
        .await;
        let hz = h
            .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
            .await
            .unwrap();
        assert!(hz.is_object());
    }
    // Writes after the restarts land in the same store.
    let mut h = host(&root).await;
    call(
        &mut h,
        method::MEMORY_REMEMBER,
        &p,
        json!({"text": "Written after restarts"}),
    )
    .await;
    drop(h);
    d.stop(&root).await;
}

/// WM-208: a full disk is a typed, survivable failure. Reads keep working,
/// the failed write leaves no row or receipt, and after the condition clears
/// the same idempotency key commits exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disk_full_is_typed_and_leaves_nothing_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    // Seed one memory with room to spare, then restart with the cap.
    let d = Daemon::start(&root, None).await;
    let mut h = host(&root).await;
    let p = register(&mut h, &proj).await;
    call(
        &mut h,
        method::MEMORY_REMEMBER,
        &p,
        json!({"text": "Seeded fact before the disk filled up"}),
    )
    .await;
    drop(h);
    d.stop(&root).await;

    let d = Daemon::start(&root, Some("disk_full")).await;
    let mut h = host(&root).await;
    let params = json!({"text": format!("Large fact that cannot fit {}", "x".repeat(3000))});
    let mut code = None;
    for i in 0..50 {
        let mut p2 = params.clone();
        p2["text"] = json!(format!("{} variant {i}", params["text"].as_str().unwrap()));
        match h
            .call_raw(
                method::MEMORY_REMEMBER,
                Some(&p),
                p2,
                Some(&format!("k-full-{i}")),
            )
            .await
        {
            Ok(_) => continue,
            Err(CallError::Remote(e)) => {
                code = Some((e.code, i));
                break;
            }
            Err(other) => panic!("expected a typed error, got {other:?}"),
        }
    }
    let (code, failed_at) = code.expect("the capped store never filled up");
    assert_eq!(code, wrongmemory_contracts::ErrorCode::DiskFull);
    // Still alive and readable.
    let health = h
        .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(health["draining"], false, "{health}");
    assert_eq!(rows_with(&mut h, &p, "Seeded fact").await.len(), 1);
    let failed_text = format!("{} variant {failed_at}", params["text"].as_str().unwrap());
    assert!(rows_with(&mut h, &p, &failed_text).await.is_empty());
    drop(h);
    d.stop(&root).await;

    // Space is back: the same key is a fresh commit (no receipt was kept).
    let d = Daemon::start(&root, None).await;
    let mut h = host(&root).await;
    let mut retry_params = params.clone();
    retry_params["text"] = json!(failed_text);
    let retry = h
        .call_raw(
            method::MEMORY_REMEMBER,
            Some(&p),
            retry_params,
            Some(&format!("k-full-{failed_at}")),
        )
        .await
        .unwrap();
    assert_eq!(retry["replayed"], false);
    assert_eq!(rows_with(&mut h, &p, &failed_text).await.len(), 1);
    drop(h);
    d.stop(&root).await;
}

/// WM-208: a project whose store vanished or was corrupted while the daemon
/// was down fails with a typed error, never takes the daemon or other
/// projects with it, and recovers when the file is restored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_or_corrupt_store_is_isolated_and_recoverable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let d = Daemon::start(&root, None).await;
    let mut h = host(&root).await;
    let a = register(&mut h, &tmp.path().join("proj-a")).await;
    let b = register(&mut h, &tmp.path().join("proj-b")).await;
    call(
        &mut h,
        method::MEMORY_REMEMBER,
        &a,
        json!({"text": "Project A survives restores"}),
    )
    .await;
    call(
        &mut h,
        method::MEMORY_REMEMBER,
        &b,
        json!({"text": "Project B is unaffected"}),
    )
    .await;
    let info = h
        .call_raw(method::PROJECT_GET, Some(&a), json!({}), None)
        .await
        .unwrap();
    let store = std::path::PathBuf::from(info["storePath"].as_str().unwrap());
    drop(h);
    d.stop(&root).await;

    let store_dir = store.parent().unwrap().to_path_buf();
    let backup = tmp.path().join("a-backup");
    for (label, damage) in [("missing", 0), ("corrupt", 1)] {
        if damage == 0 {
            std::fs::rename(&store_dir, &backup).unwrap();
        } else {
            copy_dir(&store_dir, &backup);
            std::fs::write(&store, b"this is not a sqlite database at all").unwrap();
            for ext in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", store.display()));
            }
        }
        let d = Daemon::start(&root, None).await;
        let mut h = host(&root).await;
        let e = h
            .call_raw(
                method::MEMORY_SEARCH,
                Some(&a),
                json!({"query": "survives"}),
                None,
            )
            .await
            .expect_err(&format!("{label}: a broken store must fail"));
        let CallError::Remote(e) = e else {
            panic!("{label}: expected a typed error, got {e:?}");
        };
        assert!(
            matches!(
                e.code,
                wrongmemory_contracts::ErrorCode::StoreUnavailable
                    | wrongmemory_contracts::ErrorCode::DatabaseCorrupt
                    | wrongmemory_contracts::ErrorCode::ProfileUnsupported
            ),
            "{label}: {e:?}"
        );
        // Other projects and the daemon itself are fine.
        assert_eq!(rows_with(&mut h, &b, "Project B").await.len(), 1, "{label}");
        let health = h
            .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
            .await
            .unwrap();
        assert_eq!(health["draining"], false, "{label}: {health}");
        // Restore the files: the next request reopens without a restart.
        let _ = std::fs::remove_dir_all(&store_dir);
        std::fs::rename(&backup, &store_dir).unwrap();
        assert_eq!(
            rows_with(&mut h, &a, "Project A survives").await.len(),
            1,
            "{label}: recovered"
        );
        drop(h);
        d.stop(&root).await;
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

/// WM-606: a restart elects a new daemon while the previous one is still
/// draining and holds the store's owner lock (`slow_release` widens that
/// window). The new daemon waits for its predecessor's release instead of
/// answering `OWNER_CONFLICT` (the hosted-CI flake of
/// `concurrent_bridges_share_one_native_daemon`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_waits_for_the_predecessor_to_release_its_stores() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let mut old = Daemon::start(&root, Some("slow_release")).await;
    let mut h = host(&root).await;
    let pid = register(&mut h, &tmp.path().join("P")).await;
    call(
        &mut h,
        method::MEMORY_REMEMBER,
        &pid,
        json!({"text": "Restarts keep the store owned once"}),
    )
    .await;
    let old_instance = h
        .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
        .await
        .unwrap()["daemonInstanceId"]
        .clone();
    let _ = h
        .call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await;
    drop(h);

    // Start the successor right away and wait until it, not the old one, answers.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wrongmemoryd"));
    cmd.arg("--data-root")
        .arg(&root)
        .env_remove("WRONGMEMORY_FAULT")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut new = Daemon(None);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut c = loop {
        if new.0.is_none() {
            new.0 = Some(cmd.spawn().unwrap());
        }
        if let Some(child) = new.0.as_mut()
            && child.try_wait().unwrap().is_some()
        {
            new.0 = None; // lost the election to the stopping daemon: retry
        }
        if let Ok(mut c) = Client::connect_host(&dr(&root), "successor").await
            && let Ok(v) = c
                .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
                .await
            && v["daemonInstanceId"] != old_instance
        {
            break c;
        }
        assert!(Instant::now() < deadline, "no successor daemon");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let r = c
        .call_raw(
            method::MEMORY_SEARCH,
            Some(&pid),
            json!({"query": "restarts store"}),
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("successor could not open the store: {e}"));
    assert_eq!(r["hits"].as_array().unwrap().len(), 1);
    old.0.take().map(|mut ch| ch.wait());
    drop(c);
    new.stop(&root).await;
}
