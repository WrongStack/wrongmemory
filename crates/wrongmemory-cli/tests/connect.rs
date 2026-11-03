//! Managed client configuration. Every command runs with
//! `WRONGMEMORY_CONFIG_HOME_OVERRIDE` and `--project-root` pointing into a
//! temp directory, so the real user configuration is never read or written.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    data: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    // Spaces in every path on purpose.
    let home = tmp.path().join("fake home");
    let project = tmp.path().join("my project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let data = tmp.path().join("wm data");
    Env {
        _tmp: tmp,
        home,
        project,
        data,
    }
}

impl Env {
    fn cmd(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_wrongmemory"));
        c.env("WRONGMEMORY_CONFIG_HOME_OVERRIDE", &self.home)
            .arg("--data-root")
            .arg(&self.data)
            .args(args);
        if args
            .iter()
            .any(|a| matches!(*a, "connect" | "disconnect" | "doctor"))
        {
            c.arg("--project-root").arg(&self.project);
        }
        c.output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cmd(args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

/// Stops the test's daemon when the test ends, also after a failed
/// assertion, so a red run leaves no daemon holding the binary or the temp
/// directory. Declared after the `Env`, so it runs before the directory goes.
struct StopDaemon<'a>(&'a Env);

impl Drop for StopDaemon<'_> {
    fn drop(&mut self) {
        let _ = self.0.cmd(&["daemon", "stop"]);
    }
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// A data root set only through `WRONGMEMORY_HOME` must reach the entry: the
/// client launches the bridge from its own environment, which does not have it.
#[test]
fn connect_writes_a_data_root_that_came_from_the_environment() {
    let e = env();
    let out = Command::new(env!("CARGO_BIN_EXE_wrongmemory"))
        .env("WRONGMEMORY_CONFIG_HOME_OVERRIDE", &e.home)
        .env("WRONGMEMORY_HOME", &e.data)
        .args([
            "connect",
            "claude-code",
            "--project",
            "p_1",
            "--grant",
            "g_1",
        ])
        .arg("--project-root")
        .arg(&e.project)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = read_json(&e.project.join(".mcp.json"));
    let args: Vec<&str> = v["mcpServers"]["wrongmemory"]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    let at = args
        .iter()
        .position(|a| *a == "--data-root")
        .unwrap_or_else(|| panic!("no --data-root in {args:?}"));
    let want = e.data.display().to_string().replace('\\', "/");
    assert_eq!(args[at + 1].to_lowercase(), want.to_lowercase());
}

#[test]
fn claude_project_add_idempotent_remove_preserves_unrelated() {
    let e = env();
    let cfg = e.project.join(".mcp.json");
    std::fs::write(
        &cfg,
        serde_json::to_string_pretty(&json!({
            "mcpServers": {"other": {"command": "node", "args": ["x.js"]}},
            "someSetting": true
        }))
        .unwrap(),
    )
    .unwrap();

    e.ok(&[
        "connect",
        "claude-code",
        "--project",
        "p_1",
        "--grant",
        "g_1",
    ]);
    let v = read_json(&cfg);
    assert_eq!(v["mcpServers"]["other"]["command"], "node");
    assert_eq!(v["someSetting"], true);
    let entry = &v["mcpServers"]["wrongmemory"];
    let args: Vec<&str> = entry["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(
        &args[..7],
        [
            "mcp",
            "--project",
            "p_1",
            "--grant",
            "g_1",
            "--client",
            "claude-code"
        ]
    );
    assert!(args.contains(&"--data-root"));
    assert!(
        !entry["command"].as_str().unwrap().contains('\\'),
        "forward slashes"
    );
    assert!(
        !std::fs::read_to_string(&cfg)
            .unwrap()
            .to_lowercase()
            .contains("secret")
    );
    assert!(
        cfg.with_file_name(".mcp.json.wrongmemory.bak").exists(),
        "backup of the original"
    );

    let before = std::fs::read(&cfg).unwrap();
    let out = e.ok(&[
        "--json",
        "connect",
        "claude-code",
        "--project",
        "p_1",
        "--grant",
        "g_1",
    ]);
    let plan: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(plan["changes"][0]["action"], "unchanged");
    assert_eq!(std::fs::read(&cfg).unwrap(), before, "re-run is a no-op");

    e.ok(&["disconnect", "claude-code", "--project", "p_1"]);
    let v = read_json(&cfg);
    assert!(v["mcpServers"].get("wrongmemory").is_none());
    assert_eq!(v["mcpServers"]["other"]["command"], "node");
    assert_eq!(v["someSetting"], true);
}

#[test]
fn malformed_json_is_refused_and_unchanged() {
    let e = env();
    let cfg = e.project.join(".cursor").join("mcp.json");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    let bad = "{ \"mcpServers\": { // comment\n  \"a\": 1, }\n";
    std::fs::write(&cfg, bad).unwrap();
    let out = e.cmd(&["connect", "cursor", "--project", "p", "--grant", "g"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing was changed"));
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), bad);
    let entries: Vec<_> = std::fs::read_dir(cfg.parent().unwrap()).unwrap().collect();
    assert_eq!(entries.len(), 1, "no backup or temp file written");
}

#[test]
fn dry_run_writes_nothing() {
    let e = env();
    let out = e.ok(&[
        "--json",
        "connect",
        "codex",
        "--project",
        "p",
        "--grant",
        "g",
        "--with-guidance",
        "--dry-run",
    ]);
    let plan: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(plan["dryRun"], true);
    assert_eq!(plan["changes"].as_array().unwrap().len(), 2);
    assert!(!e.project.join(".codex").exists());
    assert!(!e.project.join("AGENTS.md").exists());
}

#[test]
fn codex_user_scope_preserves_toml_comments_and_sage_is_untouched() {
    let e = env();
    let cfg = e.home.join(".codex").join("config.toml");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    let src = "# personal codex config\nmodel = \"gpt\"  # keep me\n\n[mcp_servers.sage]\ncommand = \"wstack-sage-mcp\" # legacy\nargs = []\n";
    std::fs::write(&cfg, src).unwrap();

    let out = e.ok(&[
        "connect",
        "codex",
        "--scope",
        "user",
        "--project",
        "p_9",
        "--grant",
        "g_9",
    ]);
    assert!(
        out.contains("warning"),
        "duplicate-tool warning for sage: {out}"
    );
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.starts_with("# personal codex config"));
    assert!(text.contains("# keep me") && text.contains("# legacy"));
    assert!(text.contains("[mcp_servers.wrongmemory]"));
    assert!(text.contains("startup_timeout_sec = 15"));
    // Real home untouched: the project root got nothing either.
    assert!(!e.project.join(".codex").exists());

    e.ok(&["disconnect", "codex", "--scope", "user", "--project", "p_9"]);
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        src,
        "sage entry and comments restored exactly"
    );
}

#[test]
fn guidance_block_is_owned_and_removed_cleanly() {
    let e = env();
    let agents = e.project.join("AGENTS.md");
    std::fs::write(&agents, "# Team rules\n\nAlways run tests.\n").unwrap();
    e.ok(&[
        "connect",
        "antigravity",
        "--project",
        "p",
        "--grant",
        "g",
        "--with-guidance",
    ]);
    let text = std::fs::read_to_string(&agents).unwrap();
    assert!(text.starts_with("# Team rules\n\nAlways run tests.\n"));
    assert_eq!(text.matches("<!-- wrongmemory:begin -->").count(), 1);
    e.ok(&[
        "connect",
        "antigravity",
        "--project",
        "p",
        "--grant",
        "g",
        "--with-guidance",
    ]);
    assert_eq!(
        std::fs::read_to_string(&agents).unwrap(),
        text,
        "idempotent"
    );
    assert!(e.project.join(".agents").join("mcp_config.json").exists());

    // Cursor owns its whole rule file.
    e.ok(&[
        "connect",
        "cursor",
        "--project",
        "p",
        "--grant",
        "g",
        "--with-guidance",
    ]);
    let rule = e
        .project
        .join(".cursor")
        .join("rules")
        .join("wrongmemory.mdc");
    assert!(std::fs::read_to_string(&rule).unwrap().starts_with("---\n"));

    e.ok(&["disconnect", "antigravity", "--project", "p"]);
    assert_eq!(
        std::fs::read_to_string(&agents).unwrap(),
        "# Team rules\n\nAlways run tests.\n"
    );
    e.ok(&["disconnect", "cursor", "--project", "p"]);
    assert!(!rule.exists());
}

#[test]
fn disconnect_refuses_entry_of_another_project() {
    let e = env();
    e.ok(&["connect", "claude-code", "--project", "p_a", "--grant", "g"]);
    let out = e.cmd(&["disconnect", "claude-code", "--project", "p_b"]);
    assert_eq!(out.status.code(), Some(6));
    assert!(read_json(&e.project.join(".mcp.json"))["mcpServers"]["wrongmemory"].is_object());
}

#[test]
fn print_formats_and_doctor_without_daemon() {
    let e = env();
    let json_out = e.ok(&["connect", "print", "--project", "p", "--grant", "g"]);
    let v: Value = serde_json::from_str(&json_out).unwrap();
    assert!(v["mcpServers"]["wrongmemory"]["command"].is_string());
    let toml_out = e.ok(&[
        "connect",
        "print",
        "--project",
        "p",
        "--grant",
        "g",
        "--format",
        "toml",
    ]);
    assert!(toml_out.contains("[mcp_servers.wrongmemory]"));
    assert!(!e.project.join(".mcp.json").exists(), "print edits nothing");

    e.ok(&["connect", "claude-code", "--project", "p", "--grant", "g_x"]);
    let out = e.cmd(&[
        "--json",
        "client",
        "doctor",
        "claude-code",
        "--project",
        "p",
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "fails: no daemon, no grant secret"
    );
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    let pass = |name: &str| {
        r["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == name)
            .unwrap()["pass"]
            .clone()
    };
    assert_eq!(pass("config"), true);
    assert_eq!(pass("entry"), true);
    assert_eq!(pass("executable"), true);
    assert_eq!(pass("project"), true);
    assert_eq!(pass("grant secret"), false);
    assert_eq!(pass("daemon"), false);
    assert!(
        !e.data.join("runtime").join("endpoint.json").exists(),
        "doctor never starts the daemon"
    );
}

#[cfg(windows)]
#[test]
fn create_grant_doctor_and_revoke_with_real_daemon() {
    let e = env();
    let _stop = StopDaemon(&e);
    let reg: Value = serde_json::from_str(&e.ok(&[
        "--json",
        "project",
        "add",
        e.project.to_str().unwrap(),
        "--managed",
    ]))
    .unwrap();
    let project = reg["project"]["projectId"].as_str().unwrap().to_string();
    e.ok(&["connect", "codex", "--project", &project, "--create-grant"]);
    let doc = e.ok(&["--json", "client", "doctor", "codex", "--project", &project]);
    let r: Value = serde_json::from_str(&doc).unwrap();
    assert_eq!(r["ok"], true, "{doc}");

    e.ok(&["disconnect", "codex", "--project", &project, "--revoke"]);
    let grants: Value = serde_json::from_str(&e.ok(&["--json", "grants", "list"])).unwrap();
    assert!(
        !grants["grants"][0]["revokedAtMs"].is_null(),
        "grant revoked"
    );
    // The client's bridge now fails as a credential problem: exit DENIED and
    // a message about the grant, not "service is not running" (the daemon is
    // up) and not a daemon start.
    let grant = grants["grants"][0]["grantId"].as_str().unwrap();
    let bridge = e.cmd(&["mcp", "--project", &project, "--grant", grant]);
    let stderr = String::from_utf8_lossy(&bridge.stderr);
    assert_eq!(bridge.status.code(), Some(5), "{stderr}");
    assert!(stderr.contains(&format!("grant {grant}")), "{stderr}");
    assert!(stderr.contains("revoked"), "{stderr}");
    assert!(!stderr.contains("not running"), "{stderr}");
}

/// Codex refuses tools that are not read-only in a non-interactive run, so a
/// proposal needs `--approve-proposals`. It is Codex-only, and refused for a
/// grant that could do more than propose; a refusal creates nothing.
#[test]
fn approve_proposals_is_codex_only_and_read_propose_only() {
    let e = env();
    let _stop = StopDaemon(&e);
    let reg: Value = serde_json::from_str(&e.ok(&[
        "--json",
        "project",
        "add",
        e.project.to_str().unwrap(),
        "--managed",
    ]))
    .unwrap();
    let project = reg["project"]["projectId"].as_str().unwrap().to_string();
    let grant_count = |e: &Env| -> usize {
        let v: Value = serde_json::from_str(&e.ok(&["--json", "grants", "list"])).unwrap();
        v["grants"].as_array().unwrap().len()
    };

    let claude = e.cmd(&[
        "connect",
        "claude-code",
        "--project",
        &project,
        "--create-grant",
        "--approve-proposals",
    ]);
    assert!(!claude.status.success());
    assert!(!e.project.join(".mcp.json").exists());
    assert_eq!(grant_count(&e), 0, "a refused connect created a grant");

    let curator: Value = serde_json::from_str(&e.ok(&[
        "--json",
        "grants",
        "create",
        "--label",
        "cur",
        "--profile",
        "curator",
        "--project",
        &project,
    ]))
    .unwrap();
    let curator = curator["grant"]["grantId"].as_str().unwrap().to_string();
    let refused = e.cmd(&[
        "connect",
        "codex",
        "--project",
        &project,
        "--grant",
        &curator,
        "--approve-proposals",
    ]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("read-propose"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let config = e.project.join(".codex").join("config.toml");
    assert!(!config.exists());

    e.ok(&[
        "connect",
        "codex",
        "--project",
        &project,
        "--create-grant",
        "--approve-proposals",
    ]);
    let doc: toml_edit::DocumentMut = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    for tool in ["memory_candidates", "memory_feedback"] {
        assert_eq!(
            doc["mcp_servers"]["wrongmemory"]["tools"][tool]["approval_mode"].as_str(),
            Some("approve"),
            "{doc}"
        );
    }
    assert!(
        doc["mcp_servers"]["wrongmemory"]["tools"]
            .get("memory_search")
            .is_none()
    );
}
