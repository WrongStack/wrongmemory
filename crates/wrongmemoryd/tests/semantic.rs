//! WM-508/509 against the real daemon: provider configuration, the three
//! search modes, grant limits, session privacy through the vector channel,
//! re-embedding after edits, and cache removal on disable/detach.

#![cfg(any(windows, unix))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongmemory_contracts::{ErrorCode, method};
use wrongmemory_ipc::{CallError, Client, DataRoot};

struct Daemon {
    child: Option<Child>,
}

impl Daemon {
    async fn start(root: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_wrongmemoryd"))
            .arg("--data-root")
            .arg(root)
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
        let mut c = Client::connect_host(&dr(root), "stop").await.unwrap();
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

fn ids(v: &Value) -> Vec<String> {
    v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["memory"]["id"].as_str().unwrap().to_string())
        .collect()
}

fn cache_file(root: &Path, pid: &str) -> PathBuf {
    root.join("cache")
        .join("semantic")
        .join(format!("{pid}.sqlite"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_provider_is_opt_in_bounded_and_session_safe() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let proj = tmp.path().join("P");
    std::fs::create_dir_all(&proj).unwrap();
    let daemon = Daemon::start(&root).await;
    let mut h = Client::connect_host(&dr(&root), "semantic-test")
        .await
        .unwrap();
    let pid = call(
        &mut h,
        method::PROJECT_REGISTER,
        None,
        json!({"root": proj.display().to_string(), "storeKind": "managed", "write": true}),
    )
    .await["project"]["projectId"]
        .as_str()
        .unwrap()
        .to_string();
    let p = Some(pid.as_str());
    // Unspaced CJK: only the vector channel can match a substring query.
    let shared = call(
        &mut h,
        method::MEMORY_REMEMBER,
        p,
        json!({"text": "共有キャッシュは毎晩再構築されます。"}),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    call(
        &mut h,
        method::SESSION_ACQUIRE,
        None,
        json!({"hostSessionId": "s1"}),
    )
    .await;
    let private = call(
        &mut h,
        method::MEMORY_REMEMBER,
        p,
        json!({"text": "非公開キャッシュは毎晩再構築されます。", "scope": "session"}),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    call(&mut h, method::SESSION_RELEASE, None, json!({})).await;
    let q = "キャッシュ 再構築";

    // Off by default: lexical finds nothing, auto stays lexical, required fails.
    let st = call(&mut h, method::SEMANTIC_STATUS, p, json!({})).await;
    assert_eq!(st["provider"], "none");
    assert!(!cache_file(&root, &pid).exists());
    for mode in [None, Some("lexical"), Some("auto")] {
        let mut params = json!({"query": q});
        if let Some(m) = mode {
            params["semantic"] = json!(m);
        }
        assert!(ids(&call(&mut h, method::MEMORY_SEARCH, p, params).await).is_empty());
    }
    assert_eq!(
        call_err(
            &mut h,
            method::MEMORY_SEARCH,
            p,
            json!({"query": q, "semantic": "required"})
        )
        .await,
        ErrorCode::SemanticUnavailable
    );
    assert_eq!(
        call_err(
            &mut h,
            method::MEMORY_SEARCH,
            p,
            json!({"query": q, "semantic": "always"})
        )
        .await,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        call_err(
            &mut h,
            method::SEMANTIC_CONFIGURE,
            p,
            json!({"provider": "onnx"})
        )
        .await,
        ErrorCode::InvalidRequest
    );

    // A read-only grant may read the status but not choose the provider.
    let gid = call(
        &mut h,
        method::GRANT_CREATE,
        None,
        json!({"label": "ro", "profile": "read-only", "projects": [pid]}),
    )
    .await["grant"]["grantId"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ro = Client::connect_grant(&dr(&root), &gid, "ro").await.unwrap();
    call(&mut ro, method::SEMANTIC_STATUS, p, json!({})).await;
    assert_eq!(
        call_err(
            &mut ro,
            method::SEMANTIC_CONFIGURE,
            p,
            json!({"provider": "ngram-hash"})
        )
        .await,
        ErrorCode::Forbidden
    );

    // Enable: the cache is built outside the store and covers both rows.
    let on = call(
        &mut h,
        method::SEMANTIC_CONFIGURE,
        p,
        json!({"provider": "ngram-hash"}),
    )
    .await;
    assert_eq!(on["provider"], "ngram-hash");
    assert_eq!(on["sync"]["embedded"], 2);
    assert_eq!(on["indexed"], 2);
    assert!(cache_file(&root, &pid).exists());

    // Default stays lexical (one fusion owner: a host that fuses itself).
    assert!(ids(&call(&mut h, method::MEMORY_SEARCH, p, json!({"query": q})).await).is_empty());
    // auto/required fuse, and only the caller's visible rows come back.
    for mode in ["auto", "required"] {
        let r = call(
            &mut h,
            method::MEMORY_SEARCH,
            p,
            json!({"query": q, "semantic": mode}),
        )
        .await;
        assert_eq!(r["channel"], "hybrid");
        assert_eq!(ids(&r), vec![shared.clone()], "{mode}");
        assert_eq!(r["hits"][0]["channel"], "vector");
    }
    let r = call(
        &mut ro,
        method::MEMORY_SEARCH,
        p,
        json!({"query": q, "semantic": "auto"}),
    )
    .await;
    assert_eq!(ids(&r), vec![shared.clone()], "a grant never sees s1's row");
    call(
        &mut h,
        method::SESSION_ACQUIRE,
        None,
        json!({"hostSessionId": "s1"}),
    )
    .await;
    let mine = ids(&call(
        &mut h,
        method::MEMORY_SEARCH,
        p,
        json!({"query": q, "semantic": "auto"}),
    )
    .await);
    assert!(mine.contains(&shared) && mine.contains(&private));
    call(&mut h, method::SESSION_RELEASE, None, json!({})).await;

    // An edit is re-embedded before the next fused search.
    let rev = call(&mut h, method::MEMORY_GET, p, json!({"id": shared})).await["revision"].clone();
    call(
        &mut h,
        method::MEMORY_UPDATE,
        p,
        json!({"id": shared, "expectedRevision": rev, "text": "Shared build cache is rebuilt nightly."}),
    )
    .await;
    assert!(
        ids(&call(
            &mut h,
            method::MEMORY_SEARCH,
            p,
            json!({"query": q, "semantic": "auto"})
        )
        .await)
        .is_empty()
    );
    let r = call(
        &mut h,
        method::MEMORY_SEARCH,
        p,
        json!({"query": "rebuilt nightly", "semantic": "required"}),
    )
    .await;
    assert_eq!(ids(&r), vec![shared.clone()]);

    // Disable: auto is lexical again, required fails, the cache is gone.
    let off = call(
        &mut h,
        method::SEMANTIC_CONFIGURE,
        p,
        json!({"provider": "none"}),
    )
    .await;
    assert_eq!(off["provider"], "none");
    assert!(!cache_file(&root, &pid).exists());
    assert_eq!(
        call_err(
            &mut h,
            method::MEMORY_SEARCH,
            p,
            json!({"query": q, "semantic": "required"})
        )
        .await,
        ErrorCode::SemanticUnavailable
    );

    // Detach removes a rebuilt cache too.
    call(
        &mut h,
        method::SEMANTIC_CONFIGURE,
        p,
        json!({"provider": "ngram-hash"}),
    )
    .await;
    assert!(cache_file(&root, &pid).exists());
    call(
        &mut h,
        method::PROJECT_DETACH,
        None,
        json!({"projectId": pid}),
    )
    .await;
    assert!(!cache_file(&root, &pid).exists());

    drop(ro);
    daemon.stop(&root).await;
}
