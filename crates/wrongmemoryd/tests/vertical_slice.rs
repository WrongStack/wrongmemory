//! M1/M2 vertical slice against the real daemon process, real named pipe,
//! real registry, and real SAGE SQLite stores (one of them built by the
//! WrongStack oracle). Nothing here mocks the transport, policy, or store.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongmemory_contracts::{Credential, ErrorCode, method};
use wrongmemory_ipc::{CallError, Client, DataRoot, Discovery};

struct Daemon {
    child: Option<Child>,
}

fn spawn(root: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_wrongmemoryd"))
        .arg("--data-root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wrongmemoryd")
}

impl Daemon {
    async fn start(root: &Path) -> Self {
        let child = spawn(root);
        wait_ready(root).await;
        Self { child: Some(child) }
    }

    async fn stop(mut self, root: &Path) {
        let mut c = host(root).await;
        c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
            .await
            .unwrap();
        let mut child = self.child.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "daemon exit status {status}");
                break;
            }
            assert!(Instant::now() < deadline, "daemon did not stop");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !dr(root).endpoint_file().exists(),
            "discovery not retracted"
        );
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

async fn wait_ready(root: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if Client::connect_host(&dr(root), "test-probe").await.is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "daemon did not become ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn host(root: &Path) -> Client {
    Client::connect_host(&dr(root), "test-host").await.unwrap()
}

async fn grant(root: &Path, id: &str) -> Client {
    Client::connect_grant(&dr(root), id, "test-agent")
        .await
        .unwrap()
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

async fn register_managed(c: &mut Client, root: &Path) -> String {
    let v = call(
        c,
        method::PROJECT_REGISTER,
        None,
        json!({"root": root.display().to_string(), "storeKind": "managed", "write": true}),
    )
    .await;
    v["project"]["projectId"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn singleton_election_under_concurrent_starts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let mut children: Vec<Child> = (0..8).map(|_| spawn(&root)).collect();
    wait_ready(&root).await;
    // Losers exit 3 without touching discovery; exactly one stays running.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let mut running = 0;
        for c in children.iter_mut() {
            match c.try_wait().unwrap() {
                None => running += 1,
                Some(s) => assert_eq!(s.code(), Some(3), "loser exit code"),
            }
        }
        if running == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "{running} daemons still running");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = Discovery::load(&dr(&root)).unwrap().info.pid;
    let alive: Vec<u32> = children
        .iter_mut()
        .filter_map(|c| c.try_wait().unwrap().is_none().then(|| c.id()))
        .collect();
    assert_eq!(alive, vec![pid], "discovery names the surviving daemon");
    let mut c = host(&root).await;
    call(&mut c, method::DAEMON_SHUTDOWN, None, json!({})).await;
    for mut c in children {
        let _ = c.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grants_projects_sessions_and_receipts_hold_across_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let proj_a = mkdir(tmp.path().join("A"));
    let proj_b = mkdir(tmp.path().join("B"));
    let daemon = Daemon::start(&root).await;
    let mut h = host(&root).await;

    // Bad credentials never authenticate.
    let endpoint = Discovery::load(&dr(&root)).unwrap().info.endpoint;
    let bad = Client::connect_with(
        &endpoint,
        Credential::Host {
            token: "0".repeat(64),
        },
        "intruder",
    )
    .await;
    assert!(matches!(bad, Err(CallError::Remote(e)) if e.code == ErrorCode::Unauthenticated));

    let a = register_managed(&mut h, &proj_a).await;
    let b = register_managed(&mut h, &proj_b).await;
    // Idempotent registration; a second authoritative store is refused.
    assert_eq!(register_managed(&mut h, &proj_a).await, a);
    assert_eq!(
        call_err(
            &mut h,
            method::PROJECT_REGISTER,
            None,
            json!({"root": proj_a.display().to_string(), "storeKind": "adopted"})
        )
        .await,
        ErrorCode::StoreIdentityConflict
    );

    let public = call(
        &mut h,
        method::MEMORY_REMEMBER,
        Some(&a),
        json!({"text": "Deploys use blue-green rollout with a health gate", "kind": "decision",
               "anchors": [{"type": "file", "path": "deploy/rollout.ts"}]}),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    call(
        &mut h,
        method::MEMORY_REMEMBER,
        Some(&b),
        json!({"text": "Project B deploys with a canary rollout"}),
    )
    .await;

    // Trusted host binds a named session and writes a session-private memory.
    let s = call(
        &mut h,
        method::SESSION_ACQUIRE,
        None,
        json!({"hostSessionId": "host-sess-1"}),
    )
    .await;
    assert_eq!(s["sessionId"], "host-sess-1");
    let private = call(
        &mut h,
        method::MEMORY_REMEMBER,
        Some(&a),
        json!({"text": "Session scratch: rollout investigation notes", "scope": "session"}),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Idempotent retry returns the original outcome; payload reuse is refused.
    let first = h
        .call_raw(
            method::MEMORY_REMEMBER,
            Some(&a),
            json!({"text": "Retry-safe rollout fact"}),
            Some("k-1"),
        )
        .await
        .unwrap();
    let again = h
        .call_raw(
            method::MEMORY_REMEMBER,
            Some(&a),
            json!({"text": "Retry-safe rollout fact"}),
            Some("k-1"),
        )
        .await
        .unwrap();
    assert_eq!(first["id"], again["id"]);
    assert_eq!(again["replayed"], true);
    let mismatch = h
        .call_raw(
            method::MEMORY_REMEMBER,
            Some(&a),
            json!({"text": "other"}),
            Some("k-1"),
        )
        .await;
    assert!(
        matches!(mismatch, Err(CallError::Remote(e)) if e.code == ErrorCode::IdempotencyMismatch)
    );

    // External read-propose grant scoped to project A only.
    let g = call(
        &mut h,
        method::GRANT_CREATE,
        None,
        json!({"label": "agent", "profile": "read-propose", "projects": [a]}),
    )
    .await;
    let gid = g["grant"]["grantId"].as_str().unwrap().to_string();
    assert!(g.get("secret").is_none(), "secret is never echoed");
    let mut ext = grant(&root, &gid).await;
    assert_eq!(ext.hello().principal.kind, "grant");

    let hits = call(
        &mut ext,
        method::MEMORY_SEARCH,
        Some(&a),
        json!({"query": "rollout"}),
    )
    .await;
    let ids: Vec<&str> = hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["memory"]["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&public.as_str()));
    assert!(
        !ids.contains(&private.as_str()),
        "session-private memory leaked to grant"
    );
    // Hidden and absent IDs are indistinguishable.
    assert_eq!(
        call_err(
            &mut ext,
            method::MEMORY_GET,
            Some(&a),
            json!({"id": private})
        )
        .await,
        ErrorCode::NotFound
    );
    assert_eq!(
        call_err(
            &mut ext,
            method::MEMORY_GET,
            Some(&a),
            json!({"id": "01ABSENT"})
        )
        .await,
        ErrorCode::NotFound
    );
    // Project substitution: ungranted == unknown.
    assert_eq!(
        call_err(
            &mut ext,
            method::MEMORY_SEARCH,
            Some(&b),
            json!({"query": "canary"})
        )
        .await,
        ErrorCode::ProjectNotFound
    );
    assert_eq!(
        call_err(
            &mut ext,
            method::MEMORY_SEARCH,
            Some("p_nope"),
            json!({"query": "x"})
        )
        .await,
        ErrorCode::ProjectNotFound
    );
    // Profile enforcement independent of the tool list.
    assert_eq!(
        call_err(
            &mut ext,
            method::MEMORY_REMEMBER,
            Some(&a),
            json!({"text": "forged write"})
        )
        .await,
        ErrorCode::Forbidden
    );
    assert_eq!(
        call_err(
            &mut ext,
            method::GRANT_CREATE,
            None,
            json!({"label": "x", "profile": "curator", "projects": [a]})
        )
        .await,
        ErrorCode::Forbidden
    );
    assert_eq!(
        call_err(
            &mut ext,
            method::SESSION_ACQUIRE,
            None,
            json!({"hostSessionId": "host-sess-1"})
        )
        .await,
        ErrorCode::Forbidden
    );
    let listed = call(&mut ext, method::PROJECT_LIST, None, json!({})).await;
    assert_eq!(listed["projects"].as_array().unwrap().len(), 1);

    // Proposal persists as a pending candidate, attributed to the grant.
    let prop = call(
        &mut ext,
        method::CANDIDATE_PROPOSE,
        Some(&a),
        json!({"text": "Rollback uses the previous blue environment", "targetMemoryId": public, "targetRevision": 1}),
    )
    .await;
    assert_eq!(prop["outcome"], "candidate-pending");
    let cand_id = prop["id"].as_str().unwrap().to_string();
    assert!(cand_id.starts_with("candidate_"));
    assert_eq!(
        prop["record"]["proposedBy"]["principalId"],
        format!("grant:{gid}")
    );
    // Proposals cannot target memory the proposer cannot see.
    assert_eq!(
        call_err(
            &mut ext,
            method::CANDIDATE_PROPOSE,
            Some(&a),
            json!({"text": "probe", "targetMemoryId": private})
        )
        .await,
        ErrorCode::NotFound
    );

    // Restart: committed memory, candidates, grants, and receipts survive.
    drop(ext);
    drop(h);
    daemon.stop(&root).await;
    let daemon = Daemon::start(&root).await;
    let mut h = host(&root).await;
    let replay = h
        .call_raw(
            method::MEMORY_REMEMBER,
            Some(&a),
            json!({"text": "Retry-safe rollout fact"}),
            Some("k-1"),
        )
        .await
        .unwrap();
    assert_eq!(replay["id"], first["id"], "receipt survives restart");
    assert_eq!(replay["replayed"], true);
    let cands = call(&mut h, method::CANDIDATE_LIST, Some(&a), json!({})).await;
    assert!(
        cands["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == cand_id.as_str())
    );
    // Restart expires the host's session binding: private row hidden again.
    assert_eq!(
        call_err(&mut h, method::MEMORY_GET, Some(&a), json!({"id": private})).await,
        ErrorCode::NotFound
    );
    call(
        &mut h,
        method::SESSION_ACQUIRE,
        None,
        json!({"hostSessionId": "host-sess-1"}),
    )
    .await;
    call(&mut h, method::MEMORY_GET, Some(&a), json!({"id": private})).await;

    // Revocation takes effect for an already-connected client.
    let mut ext = grant(&root, &gid).await;
    call(
        &mut ext,
        method::MEMORY_SEARCH,
        Some(&a),
        json!({"query": "rollout"}),
    )
    .await;
    call(&mut h, method::GRANT_REVOKE, None, json!({"grantId": gid})).await;
    assert_eq!(
        call_err(
            &mut ext,
            method::MEMORY_SEARCH,
            Some(&a),
            json!({"query": "rollout"})
        )
        .await,
        ErrorCode::Unauthenticated
    );
    assert!(
        Client::connect_grant(&dr(&root), &gid, "again")
            .await
            .is_err()
    );

    drop(ext);
    drop(h);
    daemon.stop(&root).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oracle_store_is_adopted_read_only_without_changing_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let project = mkdir(tmp.path().join("wrongstack-project"));
    let store_dir = mkdir(project.join(".wrongstack").join("memories"));
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/oracle/basic/sage.db");
    std::fs::copy(&fixture, store_dir.join("sage.db")).unwrap();
    // Simulate a live legacy SAGE owner.
    std::fs::write(store_dir.join("server.json"), "{}").unwrap();
    let before = std::fs::read(store_dir.join("sage.db")).unwrap();

    let daemon = Daemon::start(&root).await;
    let mut h = host(&root).await;
    let reg = call(
        &mut h,
        method::PROJECT_REGISTER,
        None,
        json!({"root": project.display().to_string(), "storeKind": "adopted", "write": true}),
    )
    .await;
    let p = reg["project"]["projectId"].as_str().unwrap().to_string();
    assert_eq!(
        reg["project"]["admission"], "read-only",
        "legacy owner metadata forces read-only"
    );
    // A second managed store for the same root is refused.
    assert_eq!(
        call_err(
            &mut h,
            method::PROJECT_REGISTER,
            None,
            json!({"root": project.display().to_string(), "storeKind": "managed"})
        )
        .await,
        ErrorCode::StoreIdentityConflict
    );
    let hits = call(
        &mut h,
        method::MEMORY_SEARCH,
        Some(&p),
        json!({"query": "bun"}),
    )
    .await;
    assert!(!hits["hits"].as_array().unwrap().is_empty());
    assert_eq!(
        call_err(
            &mut h,
            method::MEMORY_REMEMBER,
            Some(&p),
            json!({"text": "must not write"})
        )
        .await,
        ErrorCode::ReadOnly
    );
    drop(h);
    daemon.stop(&root).await;
    assert_eq!(
        before,
        std::fs::read(store_dir.join("sage.db")).unwrap(),
        "adopted store bytes changed"
    );
    assert!(
        !store_dir.join("wrongmemory-owner.lock").exists(),
        "read-only adoption took ownership"
    );
}
