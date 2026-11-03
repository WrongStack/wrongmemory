//! WM-201 / WM-INV-14: a linked Git worktree registers to its main
//! checkout's SAGE store (shared memory identity), exactly as WrongStack's
//! `resolveProjectSageStorageRoot` resolves it.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongmemory_contracts::method;
use wrongmemory_ipc::{Client, DataRoot};

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

    stop_daemon(child, c).await;
}
