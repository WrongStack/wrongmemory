//! WM-201 / WM-INV-14: a linked Git worktree registers to its main
//! checkout's SAGE store (shared memory identity), exactly as WrongStack's
//! `resolveProjectSageStorageRoot` resolves it.

#![cfg(windows)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn linked_worktree_shares_the_main_checkout_store() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
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
    let wt = tmp.path().join("linked");
    git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let store_dir = main.join(".wrongstack").join("memories");
    std::fs::create_dir_all(&store_dir).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/oracle/basic/sage.db"),
        store_dir.join("sage.db"),
    )
    .unwrap();

    let home = tmp.path().join("home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_wrongmemoryd"))
        .arg("--data-root")
        .arg(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let root = DataRoot::resolve(Some(&home)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut c = loop {
        if let Ok(c) = Client::connect_host(&root, "t").await {
            break c;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let from_worktree = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"root": wt.display().to_string(), "storeKind": "adopted"}),
            None,
        )
        .await
        .unwrap();
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
    let from_main = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"root": main.display().to_string(), "storeKind": "adopted"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(from_main["created"], false);
    assert_eq!(
        from_main["project"]["projectId"],
        from_worktree["project"]["projectId"]
    );
    let _ = c
        .call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await;
    let _ = child.wait();
}
