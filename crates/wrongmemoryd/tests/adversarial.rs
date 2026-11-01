//! WM-603 adversarial isolation against the real daemon: project
//! substitution, cross-project IDs, session spoofing, private-record
//! disclosure through every read surface, cursor reuse, hostile frames,
//! forged credentials, revocation, and profile enforcement.
//!
//! Invariant under test: a grant can never observe a session-private record
//! of another session, nor anything of a project outside its allowlist, and
//! hidden records are indistinguishable from absent ones.

#![cfg(windows)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use wrongmemory_contracts::{
    AuthRequest, Credential, ErrorCode, MAX_FRAME_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR, method,
};
use wrongmemory_ipc::{CallError, Client, DataRoot, Discovery, read_frame, write_frame};

const PRIV_MARK: &str = "zebraprivatemarker";
const CAND_MARK: &str = "okapicandidatemarker";
const B_MARK: &str = "quokkaprojectbmarker";

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
        .env_remove("WRONGMEMORY_FAULT")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Client::connect_host(&dr(root), "probe").await.is_err() {
        assert!(Instant::now() < deadline, "daemon not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Daemon(child)
}

fn dr(root: &Path) -> DataRoot {
    DataRoot::resolve(Some(root)).unwrap()
}

async fn call(c: &mut Client, m: &str, p: Option<&str>, params: Value) -> Value {
    c.call_raw(m, p, params, None)
        .await
        .unwrap_or_else(|e| panic!("{m}: {e}"))
}

async fn fail(c: &mut Client, m: &str, p: Option<&str>, params: Value) -> (ErrorCode, String) {
    match c.call_raw(m, p, params, None).await {
        Err(CallError::Remote(e)) => (e.code, e.message),
        other => panic!("{m}: expected a typed error, got {other:?}"),
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap().to_string()
}

/// Panic if any private identifier or marker appears anywhere in `v`.
fn assert_clean(what: &str, v: &Value, secrets: &[&str]) {
    let text = v.to_string().to_lowercase();
    for sec in secrets {
        assert!(
            !text.contains(&sec.to_lowercase()),
            "{what} disclosed {sec}: {text}"
        );
    }
}

struct World {
    a: String,
    b: String,
    public: String,
    private: String,
    private_cand: String,
    b_mem: String,
    rp: String,
    wr: String,
    ro_b: String,
}

async fn register(h: &mut Client, dir: &Path) -> String {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    s(&call(
        h,
        method::PROJECT_REGISTER,
        None,
        json!({"root": dir.display().to_string(), "storeKind": "managed", "write": true}),
    )
    .await["project"]["projectId"])
}

async fn grant(h: &mut Client, profile: &str, project: &str) -> String {
    s(&call(
        h,
        method::GRANT_CREATE,
        None,
        json!({"label": profile, "profile": profile, "projects": [project]}),
    )
    .await["grant"]["grantId"])
}

async fn connect(root: &Path, gid: &str) -> Client {
    Client::connect_grant(&dr(root), gid, "adversary")
        .await
        .unwrap()
}

/// Build two projects; project A holds one public memory plus a
/// session-private memory and candidate owned by host session `host-s1`.
async fn world(root: &Path, tmp: &Path, h: &mut Client) -> World {
    let a = register(h, &tmp.join("proj-a")).await;
    let b = register(h, &tmp.join("proj-b")).await;
    let public = s(&call(
        h,
        method::MEMORY_REMEMBER,
        Some(&a),
        json!({"text": "Public deploy note: the gateway validates tokens",
               "anchors": [{"type": "file", "path": "src/a.rs"}]}),
    )
    .await["id"]);
    let b_mem = s(&call(
        h,
        method::MEMORY_REMEMBER,
        Some(&b),
        json!({"text": format!("Project B gateway note {B_MARK}"),
               "anchors": [{"type": "file", "path": "src/a.rs"}]}),
    )
    .await["id"]);
    let rp = grant(h, "read-propose", &a).await;
    let wr = grant(h, "writer", &a).await;
    let ro_b = grant(h, "read-only", &b).await;
    let _ = root;
    World {
        a,
        b,
        public,
        private: String::new(),
        private_cand: String::new(),
        b_mem,
        rp,
        wr,
        ro_b,
    }
}

async fn add_private(h: &mut Client, w: &mut World) {
    call(
        h,
        method::SESSION_ACQUIRE,
        None,
        json!({"hostSessionId": "host-s1"}),
    )
    .await;
    w.private = s(&call(
        h,
        method::MEMORY_REMEMBER,
        Some(&w.a),
        json!({"text": format!("Session scratch gateway token {PRIV_MARK}"), "scope": "session",
               "anchors": [{"type": "file", "path": "src/a.rs"}, {"type": "symbol", "path": "src/a.rs", "symbol": "gateway_private"}],
               "relatedMemoryIds": [w.public], "audience": {"roles": ["reviewer"]}}),
    )
    .await["id"]);
    w.private_cand = s(&call(
        h,
        method::CANDIDATE_PROPOSE,
        Some(&w.a),
        json!({"text": format!("Session candidate gateway {CAND_MARK}"), "scope": "session",
               "anchors": [{"type": "file", "path": "src/a.rs"}]}),
    )
    .await["id"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grants_cannot_cross_projects_or_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let _d = start(&root).await;
    let mut h = Client::connect_host(&dr(&root), "host").await.unwrap();
    let mut w = world(&root, tmp.path(), &mut h).await;

    let mut rp = connect(&root, &w.rp).await;
    let mut wr = connect(&root, &w.wr).await;
    let mut ro_b = connect(&root, &w.ro_b).await;

    // Stats snapshot before any private data exists: private writes must not
    // move the counts a grant can see (counts are an existence oracle).
    let stats_before = call(&mut wr, method::MEMORY_STATS, Some(&w.a), json!({})).await;
    add_private(&mut h, &mut w).await;
    // Host rejects its own private candidate and proposes another, so the
    // event ring holds review events about session-private candidates too.
    let rejected = s(&call(
        &mut h,
        method::CANDIDATE_PROPOSE,
        Some(&w.a),
        json!({"text": format!("Rejected session draft {CAND_MARK}"), "scope": "session"}),
    )
    .await["id"]);
    call(
        &mut h,
        method::CANDIDATE_REJECT,
        Some(&w.a),
        json!({"id": rejected, "reason": "noise"}),
    )
    .await;
    let stats_after = call(&mut wr, method::MEMORY_STATS, Some(&w.a), json!({})).await;
    assert_eq!(
        stats_before, stats_after,
        "session-private writes changed a grant's stats"
    );

    let secrets = [
        w.private.as_str(),
        w.private_cand.as_str(),
        rejected.as_str(),
        w.b_mem.as_str(),
        PRIV_MARK,
        CAND_MARK,
        B_MARK,
        "host-s1",
        "gateway_private",
    ];

    // ── Project substitution on every project-scoped method ──
    let scoped = [
        method::PROJECT_GET,
        method::MEMORY_SEARCH,
        method::MEMORY_GET,
        method::MEMORY_FOR_PATH,
        method::MEMORY_REMEMBER,
        method::MEMORY_UPDATE,
        method::MEMORY_FEEDBACK,
        method::MEMORY_DELETE,
        method::MEMORY_RECORD_USAGE,
        method::MEMORY_LIST_PAGE,
        method::MEMORY_RELATED,
        method::MEMORY_FOR_AUDIENCE,
        method::MEMORY_FOR_FILE,
        method::MEMORY_GRAPH,
        method::MEMORY_STATS,
        method::MEMORY_RECOVER,
        method::MEMORY_BACKFILL,
        method::MEMORY_HYGIENE,
        method::MEMORY_LEGACY_FORGET,
        method::MEMORY_LEGACY_CONSOLIDATE,
        method::MEMORY_LEGACY_CLEAR,
        method::CANDIDATE_LIST,
        method::CANDIDATE_PROPOSE,
        method::CANDIDATE_ACCEPT,
        method::CANDIDATE_REJECT,
        method::CANDIDATE_RESOLVE,
        method::MAINTENANCE_STATUS,
        method::MAINTENANCE_CONFIGURE,
        method::MAINTENANCE_RUN,
        method::EVENTS_SINCE,
        method::JOBS_LIST,
    ];
    let probe = json!({"id": w.b_mem, "query": "gateway", "path": "src/a.rs", "text": "x",
                       "ids": [w.b_mem], "kind": "use", "generation": 0});
    for (name, c) in [("rp", &mut rp), ("wr", &mut wr)] {
        for m in scoped {
            let (other, other_msg) = fail(c, m, Some(&w.b), probe.clone()).await;
            let (unknown, unknown_msg) = fail(c, m, Some("p_doesnotexist"), probe.clone()).await;
            assert_eq!(
                (other, &other_msg),
                (unknown, &unknown_msg),
                "{name} {m}: an ungranted project is distinguishable from an unknown one"
            );
            assert!(
                matches!(other, ErrorCode::ProjectNotFound | ErrorCode::Forbidden),
                "{name} {m}: {other:?}"
            );
            assert!(!other_msg.contains(B_MARK));
        }
    }
    // read-only(B) cannot read A either.
    let (code, _) = fail(
        &mut ro_b,
        method::MEMORY_SEARCH,
        Some(&w.a),
        json!({"query": "gateway"}),
    )
    .await;
    assert_eq!(code, ErrorCode::ProjectNotFound);
    let listed = call(&mut ro_b, method::PROJECT_LIST, None, json!({})).await;
    assert_eq!(listed["projects"].as_array().unwrap().len(), 1);
    assert_clean("project.list", &listed, &[w.a.as_str()]);

    // ── Cross-project exact IDs and hidden-vs-absent ──
    for (name, c) in [("rp", &mut rp), ("wr", &mut wr)] {
        let absent = fail(
            c,
            method::MEMORY_GET,
            Some(&w.a),
            json!({"id": "01ABSENTABSENT"}),
        )
        .await;
        for id in [&w.b_mem, &w.private] {
            let got = fail(c, method::MEMORY_GET, Some(&w.a), json!({"id": id})).await;
            assert_eq!(
                got, absent,
                "{name}: hidden {id} distinguishable from absent"
            );
            let fb = fail(
                c,
                method::MEMORY_FEEDBACK,
                Some(&w.a),
                json!({"id": id, "feedback": {"verdict": "useful", "observedRevision": 1, "evidence": "probe"}}),
            )
            .await;
            let fb_absent = fail(
                c,
                method::MEMORY_FEEDBACK,
                Some(&w.a),
                json!({"id": "01ABSENTABSENT", "feedback": {"verdict": "useful", "observedRevision": 1, "evidence": "probe"}}),
            )
            .await;
            assert_eq!(fb, fb_absent, "{name}: feedback on hidden {id}");
            let used = call(
                c,
                method::MEMORY_RECORD_USAGE,
                Some(&w.a),
                json!({"ids": [id], "kind": "use"}),
            )
            .await;
            assert_eq!(used["recorded"], 0, "{name}: usage recorded on hidden {id}");
            let prop = fail(
                c,
                method::CANDIDATE_PROPOSE,
                Some(&w.a),
                json!({"text": "probe target", "targetMemoryId": id}),
            )
            .await;
            assert_eq!(prop.0, ErrorCode::NotFound, "{name}: propose target {id}");
        }
        // Hidden candidates are indistinguishable too.
        let ca = fail(
            c,
            method::CANDIDATE_REJECT,
            Some(&w.a),
            json!({"id": "candidate_absent"}),
        )
        .await;
        let cp = fail(
            c,
            method::CANDIDATE_REJECT,
            Some(&w.a),
            json!({"id": w.private_cand}),
        )
        .await;
        assert_eq!(ca.0, cp.0, "{name}: candidate visibility oracle");
    }
    // The writer may not update a hidden row either.
    let up = fail(
        &mut wr,
        method::MEMORY_UPDATE,
        Some(&w.a),
        json!({"id": w.private, "expectedRevision": 1, "importance": 0.1}),
    )
    .await;
    let up_absent = fail(
        &mut wr,
        method::MEMORY_UPDATE,
        Some(&w.a),
        json!({"id": "01ABSENTABSENT", "expectedRevision": 1, "importance": 0.1}),
    )
    .await;
    assert_eq!(up, up_absent);

    // ── Session spoofing ──
    for c in [&mut rp, &mut wr] {
        let (code, _) = fail(
            c,
            method::SESSION_ACQUIRE,
            None,
            json!({"hostSessionId": "host-s1"}),
        )
        .await;
        assert_eq!(code, ErrorCode::Forbidden);
        // Smuggled session fields in params are ignored.
        let hits = call(
            c,
            method::MEMORY_SEARCH,
            Some(&w.a),
            json!({"query": PRIV_MARK, "sessionId": "host-s1", "hostSessionId": "host-s1"}),
        )
        .await;
        assert_clean("search with smuggled session", &hits, &secrets);
    }
    // ── Private record never appears on any read surface ──
    // Fresh grant connections (no writes of their own carrying the marker).
    let mut rp = connect(&root, &w.rp).await;
    let reads: Vec<(&str, Value)> = vec![
        (method::MEMORY_SEARCH, json!({"query": PRIV_MARK})),
        (method::MEMORY_SEARCH, json!({"query": "gateway"})),
        (
            method::MEMORY_SEARCH,
            json!({"query": "session scratch gateway token"}),
        ),
        (method::MEMORY_FOR_PATH, json!({"path": "src/a.rs"})),
        (method::MEMORY_FOR_PATH, json!({"path": "src"})),
        (method::MEMORY_LIST_PAGE, json!({"limit": 200})),
        (
            method::MEMORY_LIST_PAGE,
            json!({"limit": 200, "query": PRIV_MARK}),
        ),
        (
            method::MEMORY_RELATED,
            json!({"ids": [w.public, w.private], "maxDepth": 4}),
        ),
        (
            method::MEMORY_GRAPH,
            json!({"query": "gateway", "maxDepth": 4}),
        ),
        (
            method::MEMORY_GRAPH,
            json!({"query": "src/a.rs", "maxDepth": 4}),
        ),
        (method::MEMORY_GRAPH, json!({"query": w.private})),
        (
            method::MEMORY_FOR_AUDIENCE,
            json!({"context": {"role": "reviewer"}}),
        ),
        (
            method::MEMORY_FOR_AUDIENCE,
            json!({"context": {"role": "REVIEWER"}, "limit": 1}),
        ),
        (method::MEMORY_FOR_FILE, json!({"path": "src/a.rs"})),
        (
            method::MEMORY_FOR_FILE,
            json!({"path": "src/a.rs", "includeDeleted": true, "lineStart": 1, "lineEnd": 99}),
        ),
        (method::MEMORY_STATS, json!({})),
        (method::EVENTS_SINCE, json!({"generation": 0})),
        (method::CANDIDATE_LIST, json!({})),
        (method::CANDIDATE_LIST, json!({"status": "rejected"})),
        (method::PROJECT_GET, json!({})),
    ];
    for (m, params) in reads {
        let v = call(&mut rp, m, Some(&w.a), params.clone()).await;
        assert_clean(&format!("{m} {params}"), &v, &secrets);
    }
    // The host session itself still sees its private row (sanity).
    let own = call(
        &mut h,
        method::MEMORY_GET,
        Some(&w.a),
        json!({"id": w.private}),
    )
    .await;
    assert_eq!(own["id"], w.private.as_str());
    let own_aud = call(
        &mut h,
        method::MEMORY_FOR_AUDIENCE,
        Some(&w.a),
        json!({"context": {"role": "reviewer"}}),
    )
    .await;
    assert_eq!(own_aud["memories"][0]["id"], w.private.as_str());

    // ── Cursor reuse across projects ──
    for i in 0..3 {
        call(
            &mut h,
            method::MEMORY_REMEMBER,
            Some(&w.b),
            json!({"text": format!("Project B filler entry number {i} {B_MARK}")}),
        )
        .await;
    }
    let b_page = call(
        &mut h,
        method::MEMORY_LIST_PAGE,
        Some(&w.b),
        json!({"limit": 1}),
    )
    .await;
    let b_cursor = s(&b_page["nextCursor"]);
    let reused = call(
        &mut rp,
        method::MEMORY_LIST_PAGE,
        Some(&w.a),
        json!({"limit": 200, "cursor": b_cursor}),
    )
    .await;
    assert_clean("listPage with B's cursor", &reused, &secrets);
    let a_page = call(
        &mut rp,
        method::MEMORY_LIST_PAGE,
        Some(&w.a),
        json!({"limit": 1}),
    )
    .await;
    if let Some(c) = a_page["nextCursor"].as_str() {
        let (code, _) = fail(
            &mut rp,
            method::MEMORY_LIST_PAGE,
            Some(&w.b),
            json!({"limit": 1, "cursor": c}),
        )
        .await;
        assert_eq!(code, ErrorCode::ProjectNotFound);
    }
    let forged = call(
        &mut rp,
        method::MEMORY_LIST_PAGE,
        Some(&w.a),
        json!({"limit": 200, "cursor": "eyJ1IjoiOTk5OSIsImkiOiJ6enoifQ"}),
    )
    .await;
    assert_clean("listPage with forged cursor", &forged, &secrets);

    // ── Forged feedback sessionId and proposal attribution ──
    let rev = call(
        &mut wr,
        method::MEMORY_GET,
        Some(&w.a),
        json!({"id": w.public}),
    )
    .await["revision"]
        .as_i64()
        .unwrap();
    call(
        &mut wr,
        method::MEMORY_FEEDBACK,
        Some(&w.a),
        json!({"id": w.public, "sessionId": "host-s1",
               "feedback": {"verdict": "useful", "observedRevision": rev,
                            "evidence": "verified in review", "sessionId": "host-s1",
                            "principalId": "host"}}),
    )
    .await;
    let after = call(
        &mut h,
        method::MEMORY_GET,
        Some(&w.a),
        json!({"id": w.public}),
    )
    .await;
    let fb = after["feedback"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_ne!(
        fb["sessionId"], "host-s1",
        "forged feedback session accepted: {fb}"
    );
    assert_eq!(fb["principalId"], format!("grant:{}", w.wr), "{fb}");

    let prop = call(
        &mut rp,
        method::CANDIDATE_PROPOSE,
        Some(&w.a),
        json!({"text": "The gateway retries token refresh once",
               "sources": [{"type": "user", "sessionId": "host-s1"}],
               "proposedBy": {"principalId": "host"}}),
    )
    .await;
    assert_eq!(
        prop["record"]["proposedBy"]["principalId"],
        format!("grant:{}", w.rp),
        "{prop}"
    );

    // ── Read-only profile cannot propose, give feedback, or record usage ──
    for (m, params) in [
        (method::CANDIDATE_PROPOSE, json!({"text": "ro probe"})),
        (
            method::MEMORY_FEEDBACK,
            json!({"id": w.b_mem, "feedback": {"verdict": "useful", "observedRevision": 1, "evidence": "x"}}),
        ),
        (
            method::MEMORY_RECORD_USAGE,
            json!({"ids": [w.b_mem], "kind": "use"}),
        ),
        (method::MEMORY_REMEMBER, json!({"text": "ro write"})),
    ] {
        let (code, _) = fail(&mut ro_b, m, Some(&w.b), params).await;
        assert_eq!(code, ErrorCode::Forbidden, "read-only {m}");
    }

    // A writer re-remembering the private text never merges into (and so
    // never reveals) the private row.
    for scope in ["project", "session"] {
        let r = call(
            &mut wr,
            method::MEMORY_REMEMBER,
            Some(&w.a),
            json!({"text": format!("Session scratch gateway token {PRIV_MARK}"), "scope": scope,
                   "anchors": [{"type": "file", "path": "src/a.rs"}, {"type": "symbol", "path": "src/a.rs", "symbol": "gateway_private"}]}),
        )
        .await;
        assert_ne!(
            r["id"],
            w.private.as_str(),
            "dedupe merged across sessions ({scope})"
        );
        assert_clean(
            "remember dedupe",
            &json!([r["id"], r["record"]["ownerSessionId"]]),
            &[w.private.as_str(), "host-s1"],
        );
    }

    // ── Revocation mid-connection ──
    let mut rp2 = connect(&root, &w.rp).await;
    call(
        &mut rp2,
        method::MEMORY_SEARCH,
        Some(&w.a),
        json!({"query": "gateway"}),
    )
    .await;
    call(&mut h, method::GRANT_REVOKE, None, json!({"grantId": w.rp})).await;
    let (code, _) = fail(
        &mut rp2,
        method::MEMORY_SEARCH,
        Some(&w.a),
        json!({"query": "gateway"}),
    )
    .await;
    assert_eq!(code, ErrorCode::Unauthenticated);
    let (code, _) = fail(&mut rp, method::CANDIDATE_LIST, Some(&w.a), json!({})).await;
    assert_eq!(code, ErrorCode::Unauthenticated);
    assert!(
        Client::connect_grant(&dr(&root), &w.rp, "again")
            .await
            .is_err()
    );
}

/// Visibility must apply before LIMIT: hidden session candidates newer than
/// a visible one cannot starve the visible one out of a grant's review list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_candidates_do_not_starve_visible_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let _d = start(&root).await;
    let mut h = Client::connect_host(&dr(&root), "host").await.unwrap();
    let a = register(&mut h, &tmp.path().join("proj")).await;
    let visible = s(&call(
        &mut h,
        method::CANDIDATE_PROPOSE,
        Some(&a),
        json!({"text": "Project-wide visible candidate about retries"}),
    )
    .await["id"]);
    call(
        &mut h,
        method::SESSION_ACQUIRE,
        None,
        json!({"hostSessionId": "host-s9"}),
    )
    .await;
    for i in 0..12 {
        tokio::time::sleep(Duration::from_millis(3)).await;
        call(
            &mut h,
            method::CANDIDATE_PROPOSE,
            Some(&a),
            json!({"text": format!("Private session draft number {i} about {CAND_MARK}"), "scope": "session"}),
        )
        .await;
    }
    let gid = grant(&mut h, "read-propose", &a).await;
    let mut g = connect(&root, &gid).await;
    for limit in [1, 2, 5] {
        let list = call(
            &mut g,
            method::CANDIDATE_LIST,
            Some(&a),
            json!({"limit": limit}),
        )
        .await;
        let ids: Vec<&str> = list["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![visible.as_str()], "limit {limit}: {list}");
        assert_eq!(list["truncated"], false, "limit {limit}");
        assert_clean("candidate.list", &list, &[CAND_MARK, "host-s9"]);
    }
}

async fn raw_conn(
    root: &Path,
    token: Option<&str>,
) -> BufReader<tokio::net::windows::named_pipe::NamedPipeClient> {
    let disc = Discovery::load(&dr(root)).unwrap();
    let pipe = wrongmemory_ipc::win::connect(&disc.info.endpoint, Duration::from_secs(5))
        .await
        .unwrap();
    let mut conn = BufReader::new(pipe);
    if let Some(token) = token {
        let auth = AuthRequest {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            credential: Credential::Host {
                token: token.to_string(),
            },
            client_name: "raw".into(),
            client_version: "0".into(),
        };
        write_frame(conn.get_mut(), &auth, MAX_FRAME_BYTES)
            .await
            .unwrap();
        let hello: Value = read_frame(&mut conn, MAX_FRAME_BYTES)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hello["status"], "ok", "{hello}");
    }
    conn
}

fn request(id: &str, m: &str) -> Value {
    json!({"protocol": PROTOCOL_MAJOR, "requestId": id, "method": m, "params": {}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_frames_and_forged_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let _d = start(&root).await;
    let token = Discovery::load(&dr(&root)).unwrap().token;
    let endpoint = Discovery::load(&dr(&root)).unwrap().info.endpoint;

    // Unauthenticated first frame: a request instead of authentication.
    let mut c = raw_conn(&root, None).await;
    write_frame(
        c.get_mut(),
        &request("r1", method::DAEMON_HEALTH),
        MAX_FRAME_BYTES,
    )
    .await
    .unwrap();
    let reply: Value = read_frame(&mut c, MAX_FRAME_BYTES).await.unwrap().unwrap();
    assert_eq!(reply["error"]["code"], "UNAUTHENTICATED", "{reply}");
    assert!(reply.get("result").is_none());
    // … and the connection is closed afterwards.
    let _ = write_frame(
        c.get_mut(),
        &request("r2", method::DAEMON_HEALTH),
        MAX_FRAME_BYTES,
    )
    .await;
    let next = read_frame::<_, Value>(&mut c, MAX_FRAME_BYTES).await;
    assert!(matches!(next, Ok(None) | Err(_)), "{next:?}");

    // Forged host token and forged grant secret.
    let forged = Client::connect_with(
        &endpoint,
        Credential::Host {
            token: "f".repeat(64),
        },
        "forger",
    )
    .await;
    assert!(matches!(forged, Err(CallError::Remote(e)) if e.code == ErrorCode::Unauthenticated));
    let mut h = Client::connect_host(&dr(&root), "host").await.unwrap();
    let a = register(&mut h, &tmp.path().join("proj")).await;
    let gid = grant(&mut h, "writer", &a).await;
    let forged = Client::connect_with(
        &endpoint,
        Credential::Grant {
            grant_id: gid.clone(),
            secret: "not-the-secret".into(),
        },
        "forger",
    )
    .await;
    assert!(matches!(forged, Err(CallError::Remote(e)) if e.code == ErrorCode::Unauthenticated));
    // A real grant secret presented as a host token is not a host credential.
    let secret = dr(&root).read_grant_secret(&gid).unwrap();
    let forged =
        Client::connect_with(&endpoint, Credential::Host { token: secret }, "forger").await;
    assert!(matches!(forged, Err(CallError::Remote(e)) if e.code == ErrorCode::Unauthenticated));

    // Malformed JSON: typed error, connection stays usable.
    let mut c = raw_conn(&root, Some(&token)).await;
    c.get_mut().write_all(b"{not json at all\n").await.unwrap();
    let reply: Value = read_frame(&mut c, MAX_FRAME_BYTES).await.unwrap().unwrap();
    assert_eq!(reply["error"]["code"], "INVALID_REQUEST", "{reply}");
    c.get_mut().write_all(b"[1,2,3]\n").await.unwrap();
    let reply: Value = read_frame(&mut c, MAX_FRAME_BYTES).await.unwrap().unwrap();
    assert_eq!(reply["error"]["code"], "INVALID_REQUEST", "{reply}");
    write_frame(
        c.get_mut(),
        &request("ok1", method::DAEMON_HEALTH),
        MAX_FRAME_BYTES,
    )
    .await
    .unwrap();
    let reply: Value = read_frame(&mut c, MAX_FRAME_BYTES).await.unwrap().unwrap();
    assert_eq!(reply["requestId"], "ok1");
    assert!(reply["result"].is_object(), "{reply}");

    // Oversized frame (> 4 MiB): rejected, connection closed, daemon alive.
    let mut c = raw_conn(&root, Some(&token)).await;
    let mut big = Vec::with_capacity(MAX_FRAME_BYTES + 1024);
    big.extend_from_slice(b"{\"protocol\":1,\"requestId\":\"big\",\"method\":\"daemon.health\",\"params\":{\"pad\":\"");
    big.resize(MAX_FRAME_BYTES + 512, b'a');
    big.extend_from_slice(b"\"}}\n");
    let _ = c.get_mut().write_all(&big).await;
    // Either a typed rejection followed by close, or an immediate close.
    if let Ok(Some(reply)) = read_frame::<_, Value>(&mut c, MAX_FRAME_BYTES).await {
        assert_eq!(reply["error"]["code"], "INVALID_REQUEST", "{reply}");
        assert!(
            read_frame::<_, Value>(&mut c, MAX_FRAME_BYTES)
                .await
                .map(|v| v.is_none())
                .unwrap_or(true)
        );
    }
    // Oversized authentication frame (> 16 KiB) before auth.
    let mut c = raw_conn(&root, None).await;
    let mut big = b"{\"pad\":\"".to_vec();
    big.resize(64 * 1024, b'b');
    big.extend_from_slice(b"\"}\n");
    let _ = c.get_mut().write_all(&big).await;
    let r = read_frame::<_, Value>(&mut c, MAX_FRAME_BYTES).await;
    assert!(
        !matches!(&r, Ok(Some(v)) if v["status"] == "ok"),
        "oversized auth frame authenticated: {r:?}"
    );

    let mut h = Client::connect_host(&dr(&root), "after").await.unwrap();
    let health = call(&mut h, method::DAEMON_HEALTH, None, json!({})).await;
    assert!(health.is_object());
}

/// `memory.forAudience` is a read: a read-only grant must not be able to
/// push arbitrary payloads into the store through the truncation audit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audience_context_is_bounded_before_it_reaches_the_audit_log() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let _d = start(&root).await;
    let mut h = Client::connect_host(&dr(&root), "host").await.unwrap();
    let a = register(&mut h, &tmp.path().join("proj")).await;
    // Enough audience rows that limit=1 truncates (window 5 < rows).
    for i in 0..7 {
        call(
            &mut h,
            method::MEMORY_REMEMBER,
            Some(&a),
            json!({"text": format!("Audience fact {i}: {}", ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf"][i]),
                   "audience": {"roles": ["reviewer"]}}),
        )
        .await;
    }
    let ro = grant(&mut h, "read-only", &a).await;
    let mut c = connect(&root, &ro).await;
    let big = "x".repeat(300);
    for bad in [
        json!({"context": {"role": "reviewer", "blob": "y"}}),
        json!({"context": {"role": big}}),
        json!({"context": {"role": 7}}),
        json!({"context": "reviewer"}),
    ] {
        let (code, msg) = fail(&mut c, method::MEMORY_FOR_AUDIENCE, Some(&a), bad.clone()).await;
        assert_eq!(code, ErrorCode::InvalidRequest, "{bad}");
        assert!(!msg.contains("blob") && !msg.contains(&big), "{msg}");
    }
    let ok = call(
        &mut c,
        method::MEMORY_FOR_AUDIENCE,
        Some(&a),
        json!({"context": {"role": "reviewer"}, "limit": 1}),
    )
    .await;
    assert_eq!(ok["memories"].as_array().unwrap().len(), 1);
    assert_eq!(ok["truncated"]["returned"], 1);
}

/// Legacy bulk mutations are curator-only, demand an explicit scope (or
/// `all`), and never reach rows the caller cannot see.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_bulk_mutations_are_curated_and_session_safe() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let _d = start(&root).await;
    let mut h = Client::connect_host(&dr(&root), "host").await.unwrap();
    let mut w = world(&root, tmp.path(), &mut h).await;
    add_private(&mut h, &mut w).await;
    let curator = grant(&mut h, "curator", &w.a).await;

    let mut wr = connect(&root, &w.wr).await;
    for (m, p) in [
        (method::MEMORY_LEGACY_FORGET, json!({"query": "gateway"})),
        (
            method::MEMORY_LEGACY_CONSOLIDATE,
            json!({"scope": "project-memory"}),
        ),
        (method::MEMORY_LEGACY_CLEAR, json!({"all": true})),
    ] {
        let (code, _) = fail(&mut wr, m, Some(&w.a), p).await;
        assert_eq!(code, ErrorCode::Forbidden, "writer may not call {m}");
    }

    let mut cur = connect(&root, &curator).await;
    let (code, _) = fail(&mut cur, method::MEMORY_LEGACY_CLEAR, Some(&w.a), json!({})).await;
    assert_eq!(code, ErrorCode::InvalidRequest);
    let (code, _) = fail(
        &mut cur,
        method::MEMORY_LEGACY_CLEAR,
        Some(&w.a),
        json!({"scope": "everything"}),
    )
    .await;
    assert_eq!(code, ErrorCode::InvalidRequest);

    // The grant's own session cannot see host-s1's private row, so neither
    // forget nor clear may touch it.
    let forgot = call(
        &mut cur,
        method::MEMORY_LEGACY_FORGET,
        Some(&w.a),
        json!({"query": PRIV_MARK}),
    )
    .await;
    assert_eq!(forgot["record"]["removed"], 0);
    let cleared = call(
        &mut cur,
        method::MEMORY_LEGACY_CLEAR,
        Some(&w.a),
        json!({"all": true}),
    )
    .await;
    assert_eq!(cleared["record"]["deleted"], 1, "only the public memory");
    assert_clean(
        "legacy clear result",
        &cleared,
        &[w.private.as_str(), PRIV_MARK],
    );
    let own = call(
        &mut h,
        method::MEMORY_GET,
        Some(&w.a),
        json!({"id": w.private}),
    )
    .await;
    assert_eq!(own["status"], "active");
    let public = call(
        &mut h,
        method::MEMORY_GET,
        Some(&w.a),
        json!({"id": w.public}),
    )
    .await;
    assert_eq!(public["status"], "deleted");
}

/// `memory.forFile` attaches pending-review hints to memories; a private
/// review candidate of another session must not surface there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn for_file_review_hints_respect_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let _d = start(&root).await;
    let mut h = Client::connect_host(&dr(&root), "host").await.unwrap();
    let w = world(&root, tmp.path(), &mut h).await;
    call(
        &mut h,
        method::SESSION_ACQUIRE,
        None,
        json!({"hostSessionId": "host-s1"}),
    )
    .await;
    let review = s(&call(
        &mut h,
        method::CANDIDATE_PROPOSE,
        Some(&w.a),
        json!({"text": format!("Private review of the gateway note {CAND_MARK}"), "scope": "session",
               "kind": "memory_review", "targetMemoryId": w.public, "suggestedAction": "update",
               "reviewReason": "private doubt"}),
    )
    .await["id"]);
    // The owning session sees its own hint.
    let own = call(
        &mut h,
        method::MEMORY_FOR_FILE,
        Some(&w.a),
        json!({"path": "src/a.rs"}),
    )
    .await;
    assert_eq!(own["reviewPendingCount"], 1, "{own}");
    // Another session sees the memory, but not the private review.
    let mut rp = connect(&root, &w.rp).await;
    let theirs = call(
        &mut rp,
        method::MEMORY_FOR_FILE,
        Some(&w.a),
        json!({"path": "src/a.rs"}),
    )
    .await;
    assert_eq!(theirs["totalCount"], 1, "{theirs}");
    assert_eq!(theirs["reviewPendingCount"], 0);
    assert_clean(
        "forFile review hint",
        &theirs,
        &[review.as_str(), CAND_MARK, "private doubt"],
    );
}
