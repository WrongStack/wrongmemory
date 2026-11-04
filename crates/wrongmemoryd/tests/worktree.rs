//! WM-201 / WM-INV-14: a linked Git worktree registers to its main
//! checkout's SAGE store (shared memory identity), exactly as WrongStack's
//! `resolveProjectSageStorageRoot` resolves it.

#![cfg(any(windows, unix))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongmemory_contracts::{ErrorCode, method};
use wrongmemory_ipc::{CallError, Client, DataRoot};

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

/// `<tmp>/main` with one commit and a linked worktree at `<tmp>/linked`.
fn repo_with_worktree(tmp: &Path) -> (PathBuf, PathBuf) {
    let main = tmp.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(
        &main,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    );
    let wt = tmp.join("linked");
    git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    (main, wt)
}

/// A project-local SAGE store in `checkout`, copied from the oracle fixture.
fn seed_sage_store(checkout: &Path) -> PathBuf {
    let store_dir = checkout.join(".wrongstack").join("memories");
    std::fs::create_dir_all(&store_dir).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/oracle/basic/sage.db"),
        store_dir.join("sage.db"),
    )
    .unwrap();
    store_dir
}

/// Kills the daemon when a failing assertion unwinds past it, so a red test
/// leaves no process behind (and no inherited pipe handle).
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start_daemon(home: &Path) -> (Daemon, Client) {
    let child = Daemon(
        Command::new(env!("CARGO_BIN_EXE_wrongmemoryd"))
            .arg("--data-root")
            .arg(home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let root = DataRoot::resolve(Some(home)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let c = loop {
        if let Ok(c) = Client::connect_host(&root, "t").await {
            break c;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    (child, c)
}

async fn stop_daemon(mut daemon: Daemon, mut c: Client) {
    let _ = c
        .call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await;
    let _ = daemon.0.wait();
}

async fn register(c: &mut Client, root: &Path, store_kind: &str) -> Result<Value, String> {
    c.call_raw(
        method::PROJECT_REGISTER,
        None,
        json!({"root": root.display().to_string(), "storeKind": store_kind, "write": true}),
        None,
    )
    .await
    .map_err(|e| format!("{e:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn linked_worktree_shares_the_main_checkout_store() {
    let tmp = tempfile::tempdir().unwrap();
    let (main, wt) = repo_with_worktree(tmp.path());
    let store_dir = seed_sage_store(&main);
    let (child, mut c) = start_daemon(&tmp.path().join("home")).await;

    let from_worktree = register(&mut c, &wt, "adopted").await.unwrap();
    let store = from_worktree["project"]["storePath"]
        .as_str()
        .unwrap()
        .to_lowercase();
    let expected = store_dir
        .join("sage.db")
        .display()
        .to_string()
        .to_lowercase();
    assert!(
        store.ends_with(&expected[expected.len() - 30..]) && store.contains("main"),
        "worktree resolved to {store}, expected the main checkout store {expected}"
    );
    // Registering the main checkout afterwards is the same project.
    let from_main = register(&mut c, &main, "adopted").await.unwrap();
    assert_eq!(from_main["created"], false);
    assert_eq!(
        from_main["project"]["projectId"],
        from_worktree["project"]["projectId"]
    );
    stop_daemon(child, c).await;
}

/// A host that probes only the worktree for a project-local store asks for a
/// `managed` project (WrongStack 1.4.0 does). That must still resolve to the
/// main checkout's memory, never to a second, empty one for the worktree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_registration_from_a_linked_worktree_resolves_to_the_main_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let (child, mut c) = start_daemon(&tmp.path().join("home")).await;

    // 1. The main checkout's adopted store is registered: the worktree gets it.
    let one = tmp.path().join("one");
    let (main, wt) = repo_with_worktree(&one);
    seed_sage_store(&main);
    let adopted = register(&mut c, &main, "adopted").await.unwrap();
    let from_worktree = register(&mut c, &wt, "managed").await.unwrap();
    assert_eq!(from_worktree["created"], false);
    assert_eq!(
        from_worktree["project"]["projectId"],
        adopted["project"]["projectId"]
    );
    assert_eq!(from_worktree["project"]["storeKind"], "adopted");

    // 2. Nothing registered and no store anywhere: the managed project is
    //    created for the main checkout, so the main checkout finds it too.
    let two = tmp.path().join("two");
    let (main, wt) = repo_with_worktree(&two);
    let created = register(&mut c, &wt, "managed").await.unwrap();
    assert_eq!(created["created"], true);
    assert_eq!(created["project"]["displayName"], "main");
    let from_main = register(&mut c, &main, "managed").await.unwrap();
    assert_eq!(from_main["created"], false);
    assert_eq!(
        from_main["project"]["projectId"],
        created["project"]["projectId"]
    );

    // 3. The main checkout holds an unregistered SAGE store: no managed store
    //    is created beside it; the caller is told to adopt it.
    let three = tmp.path().join("three");
    let (main, wt) = repo_with_worktree(&three);
    seed_sage_store(&main);
    let err = register(&mut c, &wt, "managed").await.unwrap_err();
    assert!(err.contains("StoreIdentityConflict"), "{err}");
    let listed = c
        .call_raw(method::PROJECT_LIST, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(listed["projects"].as_array().unwrap().len(), 2, "{listed}");

    // 4. A `.git` file planted in an unrelated directory, naming a real
    //    worktree's entry (`one`'s), does not borrow that project: Git's own
    //    back-link (`worktrees/<n>/gitdir`) names the real worktree only.
    let planted = tmp.path().join("planted");
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::copy(one.join("linked").join(".git"), planted.join(".git")).unwrap();
    for kind in ["managed", "adopted"] {
        let own = register(&mut c, &planted, kind).await;
        if let Ok(own) = &own {
            assert_ne!(
                own["project"]["projectId"], adopted["project"]["projectId"],
                "{kind}: a planted .git file borrowed the main checkout's project"
            );
        }
    }
    let own = register(&mut c, &planted, "managed").await.unwrap();
    assert!(
        own["project"]["root"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .ends_with("planted"),
        "{own}"
    );

    stop_daemon(child, c).await;
}

async fn call_err(c: &mut Client, m: &str, pid: &str, params: Value) -> ErrorCode {
    match c.call_raw(m, Some(pid), params, None).await {
        Err(CallError::Remote(e)) => e.code,
        other => panic!("{m}: expected a typed error, got {other:?}"),
    }
}

async fn ok(c: &mut Client, m: &str, pid: Option<&str>, params: Value) -> Value {
    c.call_raw(m, pid, params, None)
        .await
        .unwrap_or_else(|e| panic!("{m}: {e}"))
}

/// WM-INV-14: linked worktrees share memory, not evidence. A host bound to
/// its linked worktree reads path anchors with its own absolute paths, and
/// verification there is reported for that worktree only; the main checkout
/// keeps the shared state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bound_linked_worktree_gets_its_own_evidence_without_changing_shared_state() {
    let tmp = tempfile::tempdir().unwrap();
    let (main, wt) = repo_with_worktree(tmp.path());
    // The anchored file exists in the main checkout; the worktree renamed it.
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/config.ts"), "export const port = 8080;\n").unwrap();
    std::fs::create_dir_all(wt.join("src")).unwrap();
    std::fs::write(wt.join("src/settings.ts"), "export const port = 8080;\n").unwrap();
    let home = tmp.path().join("home");
    let (child, mut c) = start_daemon(&home).await;
    let pid = register(&mut c, &main, "managed").await.unwrap()["project"]["projectId"]
        .as_str()
        .unwrap()
        .to_string();
    let p = Some(pid.as_str());
    let id = ok(
        &mut c,
        method::MEMORY_REMEMBER,
        p,
        json!({"text": "The default port is 8080 in src/config.ts.", "kind": "fact",
               "anchors": [{"type": "file", "path": "src/config.ts"}]}),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let wt_config = wt.join("src").join("config.ts").display().to_string();
    let hits = |v: &Value| -> Vec<String> {
        v["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["memory"]["id"].as_str().unwrap().to_string())
            .collect()
    };

    // Unbound, the worktree's absolute path is outside the project root.
    let unbound = ok(
        &mut c,
        method::MEMORY_FOR_PATH,
        p,
        json!({"path": wt_config}),
    )
    .await;
    assert!(hits(&unbound).is_empty(), "{unbound}");

    let root = DataRoot::resolve(Some(&home)).unwrap();
    let mut w = Client::connect_host(&root, "worktree-host").await.unwrap();
    let wt_root = json!({"root": wt.display().to_string()});
    let bound = ok(&mut w, method::PROJECT_BIND_CHECKOUT, p, wt_root.clone()).await;
    assert_eq!(bound["checkout"]["kind"], "linkedWorktree", "{bound}");
    assert!(
        bound["checkout"]["root"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .ends_with("linked"),
        "{bound}"
    );

    // Bound, the same absolute path is the shared project path.
    let found = ok(
        &mut w,
        method::MEMORY_FOR_PATH,
        p,
        json!({"path": wt_config}),
    )
    .await;
    assert_eq!(hits(&found), vec![id.clone()], "{found}");

    // Evidence from the worktree: stale there, labeled, and not persisted.
    let before = ok(&mut c, method::MEMORY_GET, p, json!({"id": id})).await;
    for params in [json!({"id": id}), json!({"paths": [wt_config]})] {
        let r = ok(&mut w, method::MEMORY_VERIFY, p, params.clone()).await;
        let results = r["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "{params}: {r}");
        assert_eq!(results[0]["memoryId"], id.as_str());
        assert_eq!(results[0]["status"], "stale", "{params}: {r}");
        assert_eq!(results[0]["checkout"]["kind"], "linkedWorktree", "{r}");
    }
    let after = ok(&mut c, method::MEMORY_GET, p, json!({"id": id})).await;
    assert_eq!(before, after, "worktree evidence changed the shared record");

    // The main checkout's verification is the shared, persisted one.
    let r = ok(&mut c, method::MEMORY_VERIFY, p, json!({"id": id})).await;
    assert_eq!(r["results"][0]["status"], "verified", "{r}");
    assert!(r["results"][0].get("checkout").is_none(), "{r}");
    let shared = ok(&mut c, method::MEMORY_GET, p, json!({"id": id})).await;
    assert_ne!(shared, after, "main checkout evidence was not recorded");

    // Binding the main checkout clears the worktree binding.
    let main_root = json!({"root": main.display().to_string()});
    let to_main = ok(&mut w, method::PROJECT_BIND_CHECKOUT, p, main_root).await;
    assert_eq!(to_main["checkout"]["kind"], "main", "{to_main}");
    let r = ok(&mut w, method::MEMORY_VERIFY, p, json!({"id": id})).await;
    assert_eq!(r["results"][0]["status"], "verified", "{r}");
    assert!(r["results"][0].get("checkout").is_none(), "{r}");

    // A host may spell its root differently from the canonical path (an 8.3
    // short name, as on hosted CI, a junction, or a symlink such as macOS's
    // /var -> /private/var); its absolute paths use that spelling. A junction
    // (Windows) or symlink (Unix) to the worktree reproduces it.
    let alias = tmp.path().join("alias");
    #[cfg(windows)]
    let made = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&alias)
        .arg(&wt)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&wt, &alias).is_ok();
    assert!(made, "creating the alias failed");
    let mut a = Client::connect_host(&root, "alias-host").await.unwrap();
    let alias_root = json!({"root": alias.display().to_string()});
    let bound = ok(&mut a, method::PROJECT_BIND_CHECKOUT, p, alias_root).await;
    assert_eq!(bound["checkout"]["kind"], "linkedWorktree", "{bound}");
    let via_alias = alias.join("src").join("config.ts").display().to_string();
    let found = ok(
        &mut a,
        method::MEMORY_FOR_PATH,
        p,
        json!({"path": via_alias}),
    )
    .await;
    assert_eq!(hits(&found), vec![id.clone()], "{found}");

    // Refused: an unrelated directory, a planted `.git` file naming the real
    // worktree's entry, and a relative path.
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let planted = tmp.path().join("planted");
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::copy(wt.join(".git"), planted.join(".git")).unwrap();
    for dir in [&other, &planted] {
        let params = json!({"root": dir.display().to_string()});
        let code = call_err(&mut w, method::PROJECT_BIND_CHECKOUT, &pid, params).await;
        assert_eq!(code, ErrorCode::Forbidden, "{}", dir.display());
    }
    let relative = json!({"root": "linked"});
    let code = call_err(&mut w, method::PROJECT_BIND_CHECKOUT, &pid, relative).await;
    assert_eq!(code, ErrorCode::InvalidRequest);

    // A grant cannot claim a checkout, even with the curator profile.
    let grant = json!({"label": "curator", "profile": "curator", "projects": [pid]});
    let gid = ok(&mut c, method::GRANT_CREATE, None, grant).await["grant"]["grantId"]
        .as_str()
        .unwrap()
        .to_string();
    let mut g = Client::connect_grant(&root, &gid, "agent").await.unwrap();
    let code = call_err(&mut g, method::PROJECT_BIND_CHECKOUT, &pid, wt_root).await;
    assert_eq!(code, ErrorCode::Forbidden);

    stop_daemon(child, c).await;
}
