//! Operations commands against real binaries and a real daemon. Everything
//! uses temp data roots, a temp install prefix, and an override autostart
//! location: a registry key under HKCU\Software\WrongMemoryTest that is
//! removed afterwards (Windows), or a temp unit/agent directory (Unix).

#![cfg(any(windows, unix))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn wm(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_wrongmemory"));
    c.arg("--data-root").arg(home);
    c
}

fn run(home: &Path, args: &[&str]) -> Output {
    wm(home).args(args).output().unwrap()
}

fn json(home: &Path, args: &[&str]) -> Value {
    let out = wm(home).arg("--json").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "wrongmemory {args:?} failed ({:?}): {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Executable file name on this platform.
fn exe(base: &str) -> String {
    format!("{base}{}", std::env::consts::EXE_SUFFIX)
}

/// Rollback or staging sibling: `wrongmemory.exe.old`, `wrongmemory.old`.
fn tagged(base: &str, tag: &str) -> String {
    format!("{}.{tag}", exe(base))
}

fn sha(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn setup(tmp: &Path) -> (PathBuf, String, String) {
    let home = tmp.join("home");
    let proj = tmp.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let reg = json(
        &home,
        &["project", "add", proj.to_str().unwrap(), "--managed"],
    );
    let p = reg["project"]["projectId"].as_str().unwrap().to_string();
    let store = reg["project"]["storePath"].as_str().unwrap().to_string();
    (home, p, store)
}

fn stop(home: &Path) {
    let _ = run(home, &["daemon", "stop"]);
    for _ in 0..100 {
        if !home.join("runtime").join("endpoint.json").exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn backup_while_running_is_consistent_and_never_overwrites() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, p, store) = setup(tmp.path());
    json(
        &home,
        &[
            "remember",
            &p,
            "Backups are taken with the SQLite online backup API",
        ],
    );
    let store = PathBuf::from(store);
    let wal = store.with_extension("db-wal");
    let before = (sha(&store), sha(&wal));
    let out = tmp.path().join("snap").join("b1.db");
    let r = json(&home, &["backup", &p, "--output", out.to_str().unwrap()]);
    assert_eq!(r["integrity"], "ok");
    assert_eq!(r["verdict"], "compatible");
    assert_eq!(r["memoryRows"], 1, "WAL content must be in the snapshot");
    assert_eq!(
        before,
        (sha(&store), sha(&wal)),
        "backup modified the source"
    );
    let again = run(&home, &["backup", &p, "--output", out.to_str().unwrap()]);
    assert_eq!(again.status.code(), Some(2), "overwrite must be refused");
    // Offline path by explicit store.
    stop(&home);
    let out2 = tmp.path().join("snap").join("b2.db");
    let r2 = json(
        &home,
        &[
            "backup",
            "--store",
            store.to_str().unwrap(),
            "--output",
            out2.to_str().unwrap(),
        ],
    );
    assert_eq!(r2["memoryRows"], 1);
    let plan = json(&home, &["migrate", "inspect", out2.to_str().unwrap()]);
    assert_eq!(plan["inspection"]["verdict"], "compatible");
    assert!(!plan["plan"].as_array().unwrap().is_empty());
}

#[test]
fn diagnostics_contain_no_memory_text() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, p, _) = setup(tmp.path());
    let marker = "ZebraMarker7731 durable fact about rollout ordering";
    json(&home, &["remember", &p, marker]);
    json(&home, &["search", &p, "ZebraMarker7731"]);
    let out = tmp.path().join("diag");
    let r = json(
        &home,
        &["diagnostics", "export", "--output", out.to_str().unwrap()],
    );
    let files = r["files"].as_array().unwrap();
    assert!(files.iter().any(|f| f == "projects.json"));
    assert!(files.iter().any(|f| f == "daemon-log-tail.txt"));
    let token = std::fs::read_to_string(home.join("runtime").join("token")).unwrap();
    for entry in std::fs::read_dir(&out).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(
            !text.contains("ZebraMarker7731"),
            "memory text leaked into diagnostics"
        );
        assert!(
            !text.contains(token.trim()),
            "host token leaked into diagnostics"
        );
    }
    let busy = run(
        &home,
        &["diagnostics", "export", "--output", out.to_str().unwrap()],
    );
    assert_eq!(busy.status.code(), Some(2));
    stop(&home);
}

#[test]
fn install_refuses_running_daemon_then_installs_and_uninstalls() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _, store) = setup(tmp.path());
    let prefix = tmp.path().join("prefix with space").join("bin");
    let refused = run(&home, &["install", "--prefix", prefix.to_str().unwrap()]);
    assert_eq!(
        refused.status.code(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let r = json(
        &home,
        &[
            "install",
            "--prefix",
            prefix.to_str().unwrap(),
            "--stop-daemon",
        ],
    );
    assert_eq!(r["daemonStopped"], true);
    assert!(prefix.join(exe("wrongmemory")).exists() && prefix.join(exe("wrongmemoryd")).exists());
    // The installed CLI starts its own installed daemon on demand.
    let installed = prefix.join(exe("wrongmemory"));
    let st = Command::new(&installed)
        .args([
            "--data-root",
            home.to_str().unwrap(),
            "--json",
            "daemon",
            "start",
        ])
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );
    // Reinstall over an existing install keeps a rollback copy.
    json(
        &home,
        &[
            "install",
            "--prefix",
            prefix.to_str().unwrap(),
            "--stop-daemon",
        ],
    );
    assert!(prefix.join(tagged("wrongmemory", "old")).exists());
    json(&home, &["daemon", "start"]);
    let refuse_data = run(
        &home,
        &[
            "uninstall",
            "--prefix",
            prefix.to_str().unwrap(),
            "--remove-data",
        ],
    );
    assert_eq!(
        refuse_data.status.code(),
        Some(2),
        "--remove-data needs --yes"
    );
    let u = json(&home, &["uninstall", "--prefix", prefix.to_str().unwrap()]);
    assert_eq!(u["dataKept"], true);
    assert!(!prefix.join(exe("wrongmemory")).exists());
    assert!(!prefix.join(tagged("wrongmemoryd", "old")).exists());
    assert!(Path::new(&store).exists(), "uninstall must keep memory");
    assert!(
        !home.join("runtime").join("endpoint.json").exists(),
        "daemon stopped"
    );
}

#[cfg(windows)]
#[test]
fn autostart_enable_status_disable_with_override_key() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let prefix = tmp.path().join("bin");
    std::fs::create_dir_all(&prefix).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_BIN_EXE_wrongmemory")).with_file_name("wrongmemoryd.exe"),
        prefix.join("wrongmemoryd.exe"),
    )
    .unwrap();
    let key = format!(r"HKCU\Software\WrongMemoryTest\Run{}", std::process::id());
    let with_key = |args: &[&str]| -> Value {
        let out = wm(&home)
            .env("WRONGMEMORY_AUTOSTART_KEY_OVERRIDE", &key)
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    assert!(with_key(&["autostart", "status"])["command"].is_null());
    let e = with_key(&["autostart", "enable", "--prefix", prefix.to_str().unwrap()]);
    assert_eq!(e["changed"], true);
    let cmd = e["command"].as_str().unwrap().to_string();
    assert!(cmd.contains("wrongmemoryd.exe") && cmd.contains("--data-root"));
    assert_eq!(
        with_key(&["autostart", "enable", "--prefix", prefix.to_str().unwrap()])["changed"],
        false
    );
    assert_eq!(with_key(&["autostart", "status"])["command"], cmd.as_str());
    assert_eq!(with_key(&["autostart", "disable"])["changed"], true);
    assert_eq!(with_key(&["autostart", "disable"])["changed"], false);
    let _ = Command::new("reg").args(["delete", &key, "/f"]).output();
    let _ = Command::new("reg")
        .args(["delete", r"HKCU\Software\WrongMemoryTest", "/f"])
        .output();
}

/// WM-802: the sign-in entry is a systemd user unit linked into
/// `default.target.wants` (Linux) or a LaunchAgent (macOS). The directory is
/// overridden, which also keeps `systemctl`/`launchctl` out of the test.
#[cfg(unix)]
#[test]
fn autostart_enable_status_disable_and_uninstall_with_override_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let prefix = tmp.path().join("my bin");
    let units = tmp.path().join("units");
    std::fs::create_dir_all(&prefix).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_BIN_EXE_wrongmemory")).with_file_name("wrongmemoryd"),
        prefix.join("wrongmemoryd"),
    )
    .unwrap();
    let with_dir = |args: &[&str]| -> Value {
        let out = wm(&home)
            .env("WRONGMEMORY_AUTOSTART_DIR_OVERRIDE", &units)
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    #[cfg(target_os = "macos")]
    let file = units.join("com.wrongstack.wrongmemory.plist");
    #[cfg(not(target_os = "macos"))]
    let file = units.join("wrongmemory.service");

    assert!(with_dir(&["autostart", "status"])["command"].is_null());
    let e = with_dir(&["autostart", "enable", "--prefix", prefix.to_str().unwrap()]);
    assert_eq!(e["changed"], true);
    assert_eq!(e["file"], file.display().to_string());
    let cmd = e["command"].as_str().unwrap().to_string();
    assert!(
        cmd.contains("my bin/wrongmemoryd") && cmd.contains("--data-root"),
        "{cmd}"
    );
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains(&home.display().to_string()), "{text}");
    #[cfg(not(target_os = "macos"))]
    {
        let link = units
            .join("default.target.wants")
            .join("wrongmemory.service");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("..").join("wrongmemory.service")
        );
        assert!(text.contains("WantedBy=default.target"), "{text}");
    }
    assert_eq!(
        with_dir(&["autostart", "enable", "--prefix", prefix.to_str().unwrap()])["changed"],
        false
    );
    assert_eq!(with_dir(&["autostart", "status"])["command"], cmd.as_str());
    assert_eq!(with_dir(&["autostart", "disable"])["changed"], true);
    assert!(!file.exists());
    assert_eq!(with_dir(&["autostart", "disable"])["changed"], false);

    // Uninstall removes an entry that starts the daemon from its prefix.
    with_dir(&["autostart", "enable", "--prefix", prefix.to_str().unwrap()]);
    let u = with_dir(&["uninstall", "--prefix", prefix.to_str().unwrap()]);
    assert!(
        u["removed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().starts_with("autostart entry")),
        "{u}"
    );
    assert!(!file.exists());
    assert!(!prefix.join("wrongmemoryd").exists());
}

/// WM-701: an install interrupted after the first binary was replaced
/// rolls that binary back, so the prefix never holds a mixed CLI/daemon
/// pair. The second replacement is made to fail by occupying its staging
/// path with a directory.
#[test]
fn interrupted_install_rolls_back_to_the_previous_binaries() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let prefix = tmp.path().join("prefix");
    std::fs::create_dir_all(&prefix).unwrap();
    let cli = prefix.join(exe("wrongmemory"));
    let daemon = prefix.join(exe("wrongmemoryd"));
    std::fs::write(&cli, b"previous cli").unwrap();
    std::fs::write(&daemon, b"previous daemon").unwrap();
    std::fs::create_dir_all(prefix.join(tagged("wrongmemoryd", "new"))).unwrap();

    let out = run(&home, &["install", "--prefix", prefix.to_str().unwrap()]);
    assert!(!out.status.success(), "install must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("rolled back"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(sha(&cli).as_deref(), Some(&b"previous cli"[..]));
    assert_eq!(sha(&daemon).as_deref(), Some(&b"previous daemon"[..]));
    assert!(!prefix.join("install.json").exists());

    // Clearing the obstruction lets the same install complete.
    std::fs::remove_dir(prefix.join(tagged("wrongmemoryd", "new"))).unwrap();
    json(&home, &["install", "--prefix", prefix.to_str().unwrap()]);
    assert_eq!(sha(&cli), sha(Path::new(env!("CARGO_BIN_EXE_wrongmemory"))));
    assert_eq!(
        sha(&prefix.join(tagged("wrongmemory", "old"))).as_deref(),
        Some(&b"previous cli"[..])
    );
}
