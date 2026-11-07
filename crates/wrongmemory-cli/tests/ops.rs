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
    // `install` must never edit this machine's PATH from a test; the PATH
    // test below lifts this and redirects every location instead.
    c.env("WRONGMEMORY_NO_MODIFY_PATH", "1");
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

/// `install` puts its prefix on the user PATH once, keeps what was there, and
/// `uninstall` takes only its own entry out again. Every location is
/// redirected: a scratch registry key (Windows) or a temp `$HOME` (Unix).
#[test]
fn install_puts_the_prefix_on_path_and_uninstall_takes_it_off() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let prefix = tmp.path().join("pre fix").join("bin");
    let with_path = |args: &[&str]| -> Value {
        let mut c = Command::new(env!("CARGO_BIN_EXE_wrongmemory"));
        c.arg("--data-root").arg(&data).arg("--json").args(args);
        c.env_remove("WRONGMEMORY_NO_MODIFY_PATH");
        redirect(&mut c, tmp.path());
        let out = c.output().unwrap();
        assert!(
            out.status.success(),
            "wrongmemory {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let p = prefix.to_str().unwrap();
    let _cleanup = seed(tmp.path());

    let first = with_path(&["install", "--prefix", p]);
    assert_eq!(first["path"]["modified"], true, "{first}");
    let again = with_path(&["install", "--prefix", p]);
    assert_eq!(
        again["path"]["modified"], false,
        "a second install changes nothing: {again}"
    );
    check_installed(tmp.path(), &prefix);

    // Opting out leaves everything as it is.
    let skipped = with_path(&["install", "--prefix", p, "--no-modify-path"]);
    assert_eq!(skipped["path"]["skipped"], true);

    let u = with_path(&["uninstall", "--prefix", p]);
    assert!(
        u["removed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().starts_with("PATH entry")),
        "{u}"
    );
    check_removed(tmp.path(), &prefix);
}

#[cfg(windows)]
fn path_key(tmp: &Path) -> String {
    // Its own root: the autostart test deletes HKCU\Software\WrongMemoryTest.
    let leaf = tmp.file_name().unwrap().to_string_lossy().to_string();
    format!(r"Software\WrongMemoryPathTest\{leaf}")
}

#[cfg(windows)]
struct DropKey(String);

#[cfg(windows)]
impl Drop for DropKey {
    fn drop(&mut self) {
        // The whole scratch root: only this test uses it.
        let root = self.0.rsplit_once('\\').map_or(self.0.as_str(), |(r, _)| r);
        let _ = Command::new("reg")
            .args(["delete", &format!(r"HKCU\{root}"), "/f"])
            .output();
    }
}

#[cfg(windows)]
fn redirect(c: &mut Command, tmp: &Path) {
    c.env("WRONGMEMORY_ENV_KEY_OVERRIDE", path_key(tmp));
}

#[cfg(windows)]
fn reg_path(tmp: &Path) -> String {
    let out = Command::new("reg")
        .args(["query", &format!(r"HKCU\{}", path_key(tmp)), "/v", "Path"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[cfg(windows)]
fn seed(tmp: &Path) -> DropKey {
    let key = DropKey(path_key(tmp));
    let ok = Command::new("reg")
        .args([
            "add",
            &format!(r"HKCU\{}", path_key(tmp)),
            "/v",
            "Path",
            "/t",
            "REG_EXPAND_SZ",
            "/d",
            r"%SystemRoot%\system32;C:\Other Tools",
            "/f",
        ])
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok);
    key
}

#[cfg(windows)]
fn check_installed(tmp: &Path, prefix: &Path) {
    let q = reg_path(tmp);
    assert!(q.contains("REG_EXPAND_SZ"), "type kept: {q}");
    assert!(
        q.contains(r"%SystemRoot%\system32"),
        "entries stay unexpanded: {q}"
    );
    assert!(q.contains(r"C:\Other Tools"), "{q}");
    let p = prefix.display().to_string();
    assert_eq!(q.matches(&p).count(), 1, "added exactly once: {q}");
}

#[cfg(windows)]
fn check_removed(tmp: &Path, prefix: &Path) {
    let q = reg_path(tmp);
    assert!(!q.contains(&prefix.display().to_string()), "{q}");
    assert!(q.contains(r"%SystemRoot%\system32;C:\Other Tools"), "{q}");
}

#[cfg(unix)]
fn redirect(c: &mut Command, tmp: &Path) {
    c.env("HOME", tmp.join("user"))
        .env("SHELL", "/bin/bash")
        .env_remove("ZDOTDIR")
        .env_remove("XDG_CONFIG_HOME");
}

#[cfg(unix)]
const USER_RC: &str = "alias ll='ls -l'\n";

#[cfg(unix)]
fn seed(tmp: &Path) -> PathBuf {
    let home = tmp.join("user");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".bashrc"), USER_RC).unwrap();
    home
}

#[cfg(unix)]
fn check_installed(tmp: &Path, prefix: &Path) {
    let home = tmp.join("user");
    for f in [".profile", ".bashrc"] {
        let text = std::fs::read_to_string(home.join(f)).unwrap();
        assert_eq!(
            text.matches("# >>> wrongmemory >>>").count(),
            1,
            "{f}: {text}"
        );
    }
    assert!(
        std::fs::read_to_string(home.join(".bashrc"))
            .unwrap()
            .starts_with(USER_RC)
    );
    // A login shell finds the installed CLI, even with a space in the prefix,
    // and sourcing the profile twice does not add the directory twice.
    let out = Command::new("sh")
        .arg("-c")
        .arg(". \"$HOME/.profile\"; . \"$HOME/.profile\"; command -v wrongmemory; printf '%s\n' \"$PATH\"")
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let p = prefix.display().to_string();
    assert!(text.starts_with(&format!("{p}/wrongmemory")), "{text}");
    assert_eq!(text.matches(&format!("{p}:")).count(), 1, "{text}");
}

#[cfg(unix)]
fn check_removed(tmp: &Path, _prefix: &Path) {
    let home = tmp.join("user");
    assert_eq!(
        std::fs::read_to_string(home.join(".bashrc")).unwrap(),
        USER_RC,
        "only our block is removed"
    );
    let profile = std::fs::read_to_string(home.join(".profile")).unwrap();
    assert!(!profile.contains("wrongmemory"), "{profile}");
}
