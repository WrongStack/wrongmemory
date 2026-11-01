//! Review/lifecycle flow through the real daemon: an external read-propose
//! client proposes, a curator (host) accepts, the accepted revision is
//! visible to the external client, writer CAS conflicts, feedback is
//! attributed, delete needs a destructive grant, recover restores, graph and
//! listing never disclose a session-private memory.

#![cfg(windows)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongmemory_contracts::{ErrorCode, method};
use wrongmemory_ipc::{CallError, Client, DataRoot};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start(root: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_wrongmemoryd"))
        .arg("--data-root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Client::connect_host(&dr(root), "probe").await.is_err() {
        assert!(Instant::now() < deadline, "daemon not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Daemon(child)
}

fn dr(root: &Path) -> DataRoot {
    DataRoot::resolve(Some(root)).unwrap()
}

async fn call(c: &mut Client, m: &str, p: &str, params: Value) -> Value {
    c.call_raw(m, Some(p), params, None)
        .await
        .unwrap_or_else(|e| panic!("{m}: {e}"))
}

async fn code(c: &mut Client, m: &str, p: &str, params: Value) -> ErrorCode {
    match c.call_raw(m, Some(p), params, None).await {
        Err(CallError::Remote(e)) => e.code,
        other => panic!("{m}: expected error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_and_lifecycle_through_the_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(proj.join("src/lib.rs"), "pub fn handler() {}").unwrap();
    let _d = start(&root).await;
    let mut host = Client::connect_host(&dr(&root), "host").await.unwrap();
    let reg = host
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"root": proj.display().to_string(), "storeKind": "managed"}),
            None,
        )
        .await
        .unwrap();
    let p = reg["project"]["projectId"].as_str().unwrap().to_string();
    let mk = |profile: &str| json!({"label": profile, "profile": profile, "projects": [p]});
    let mut grants = Vec::new();
    for profile in ["read-propose", "writer"] {
        let g = host
            .call_raw(method::GRANT_CREATE, None, mk(profile), None)
            .await
            .unwrap();
        grants.push(g["grant"]["grantId"].as_str().unwrap().to_string());
    }
    let mut agent = Client::connect_grant(&dr(&root), &grants[0], "agent")
        .await
        .unwrap();
    let mut writer = Client::connect_grant(&dr(&root), &grants[1], "writer")
        .await
        .unwrap();

    // External proposal -> curator accept -> visible to the proposer.
    let prop = call(
        &mut agent,
        method::CANDIDATE_PROPOSE,
        &p,
        json!({"text": "The request handler validates the auth header before routing",
               "anchors": [{"type": "file", "path": "src/lib.rs"}]}),
    )
    .await;
    let cid = prop["id"].as_str().unwrap().to_string();
    assert_eq!(
        code(&mut agent, method::CANDIDATE_ACCEPT, &p, json!({"id": cid})).await,
        ErrorCode::Forbidden,
        "a proposer cannot accept its own proposal"
    );
    assert_eq!(
        code(
            &mut agent,
            method::CANDIDATE_PROPOSE,
            &p,
            json!({"text": "Session scratch note for later", "scope": "session"})
        )
        .await,
        ErrorCode::InvalidRequest
    );
    let acc = call(&mut host, method::CANDIDATE_ACCEPT, &p, json!({"id": cid})).await;
    let mid = acc["memoryId"].as_str().unwrap().to_string();
    let again = call(&mut host, method::CANDIDATE_ACCEPT, &p, json!({"id": cid})).await;
    assert_eq!(again["alreadyResolved"], true);
    let seen = call(&mut agent, method::MEMORY_GET, &p, json!({"id": mid})).await;
    assert_eq!(seen["revision"], 1);
    let hits = call(
        &mut agent,
        method::MEMORY_FOR_PATH,
        &p,
        json!({"path": "src/lib.rs"}),
    )
    .await;
    assert_eq!(hits["hits"][0]["memory"]["id"], mid.as_str());

    // Writer CAS: stale expectedRevision conflicts.
    let up = call(
        &mut writer,
        method::MEMORY_UPDATE,
        &p,
        json!({"id": mid, "expectedRevision": 1, "importance": 0.9}),
    )
    .await;
    assert_eq!(up["revision"], 2);
    assert_eq!(
        code(
            &mut writer,
            method::MEMORY_UPDATE,
            &p,
            json!({"id": mid, "expectedRevision": 1, "importance": 0.1})
        )
        .await,
        ErrorCode::RevisionConflict
    );
    assert_eq!(
        code(
            &mut agent,
            method::MEMORY_UPDATE,
            &p,
            json!({"id": mid, "expectedRevision": 2, "importance": 0.1})
        )
        .await,
        ErrorCode::Forbidden
    );
    // Writers cannot delete through update.
    assert_eq!(
        code(
            &mut writer,
            method::MEMORY_UPDATE,
            &p,
            json!({"id": mid, "status": "deleted"})
        )
        .await,
        ErrorCode::Forbidden
    );

    // Feedback: attributed, revision-bound, does not change content.
    let fb = call(
        &mut agent,
        method::MEMORY_FEEDBACK,
        &p,
        json!({"id": mid, "feedback": {"verdict": "incorrect", "observedRevision": 2, "evidence": "handler routes before checking auth in src/lib.rs"}}),
    )
    .await;
    assert_eq!(fb["revision"], 2);
    let fbk = &fb["record"]["feedback"][0];
    assert_eq!(fbk["principalId"], format!("grant:{}", grants[0]));
    assert!(fbk["sessionId"].as_str().unwrap().starts_with("ses_"));
    assert_eq!(
        code(&mut agent, method::MEMORY_FEEDBACK, &p, json!({"id": mid, "feedback": {"verdict": "useful", "observedRevision": 1, "evidence": "x"}})).await,
        ErrorCode::RevisionConflict
    );

    // Session-private memory never appears in listings or graphs for others.
    call(
        &mut host,
        method::SESSION_ACQUIRE,
        "",
        json!({"hostSessionId": "h1"}),
    )
    .await;
    let private = call(
        &mut host,
        method::MEMORY_REMEMBER,
        &p,
        json!({"text": "Private scratch about the handler auth investigation", "scope": "session",
               "anchors": [{"type": "file", "path": "src/lib.rs"}]}),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let page = call(&mut agent, method::MEMORY_LIST_PAGE, &p, json!({})).await;
    assert!(!page.to_string().contains(&private));
    let graph = call(
        &mut agent,
        method::MEMORY_GRAPH,
        &p,
        json!({"query": "src/lib.rs"}),
    )
    .await;
    assert!(graph.to_string().contains(&mid));
    assert!(
        !graph.to_string().contains(&private),
        "graph leaked a hidden memory"
    );
    let stats = call(&mut agent, method::MEMORY_STATS, &p, json!({})).await;
    assert_eq!(stats["byStatus"]["active"], 1);

    // Usage telemetry never bumps revision.
    let u = call(
        &mut agent,
        method::MEMORY_RECORD_USAGE,
        &p,
        json!({"ids": [mid, private], "kind": "use"}),
    )
    .await;
    assert_eq!(u["recorded"], 1, "hidden ids are silently skipped");
    assert_eq!(
        call(&mut agent, method::MEMORY_GET, &p, json!({"id": mid})).await["revision"],
        2
    );

    // Delete: curator + force; then recover.
    assert_eq!(
        code(
            &mut host,
            method::MEMORY_DELETE,
            &p,
            json!({"id": mid, "reason": "test"})
        )
        .await,
        ErrorCode::Forbidden
    );
    let del = call(
        &mut host,
        method::MEMORY_DELETE,
        &p,
        json!({"id": mid, "reason": "obsolete", "force": true}),
    )
    .await;
    assert_eq!(del["record"]["status"], "deleted");
    let rec = call(&mut host, method::MEMORY_RECOVER, &p, json!({"id": mid})).await;
    assert_eq!(rec["status"], "active");

    // Hygiene dry run reports without writing.
    let h = call(
        &mut host,
        method::MEMORY_HYGIENE,
        &p,
        json!({"dryRun": true}),
    )
    .await;
    assert_eq!(h["dryRun"], true);
    assert_eq!(
        code(
            &mut agent,
            method::MEMORY_HYGIENE,
            &p,
            json!({"dryRun": true})
        )
        .await,
        ErrorCode::Forbidden
    );

    let mut c = Client::connect_host(&dr(&root), "stop").await.unwrap();
    c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await
        .unwrap();
}
