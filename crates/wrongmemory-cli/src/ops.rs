//! Operations commands (WM-600, WM-700..703): backup, migration inspection,
//! redacted diagnostics, per-user autostart, install, uninstall.
//!
//! Every system-touching path is overridable so tests never modify the real
//! machine: `--prefix` for binaries, `--data-root` for data,
//! `WRONGMEMORY_AUTOSTART_KEY_OVERRIDE` for the Windows Run key, and
//! `WRONGMEMORY_AUTOSTART_DIR_OVERRIDE` for the Unix unit or agent directory,
//! `WRONGMEMORY_ENV_KEY_OVERRIDE` for the Windows user environment key and
//! `$HOME` for the Unix shell startup files (see `user_path`).

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use serde_json::{Value, json};
use wrongmemory_contracts::{exit_code, method};
use wrongmemory_ipc::{Client, DataRoot};
use wrongmemory_storage::inspect::Verdict;

#[cfg(windows)]
pub const AUTOSTART_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
pub const AUTOSTART_KEY_ENV: &str = "WRONGMEMORY_AUTOSTART_KEY_OVERRIDE";
#[cfg(windows)]
pub const AUTOSTART_VALUE: &str = "WrongMemory";
const BINARIES: [&str; 2] = ["wrongmemory", "wrongmemoryd"];
const MAX_LOG_LINES: usize = 500;

#[derive(Debug, Args)]
pub struct BackupArgs {
    /// Registered project id (resolved through the running daemon).
    pub project: Option<String>,
    /// Explicit store path (works with the daemon stopped).
    #[arg(long)]
    pub store: Option<PathBuf>,
    /// Destination file; must not exist.
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum MigrateCmd {
    /// Read-only: inspect a store and print what admission would do.
    Inspect { store: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum DiagnosticsCmd {
    /// Write a bounded, redacted diagnostics bundle (no memory text, no secrets).
    Export {
        /// Output directory; must not exist or be empty.
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum AutostartCmd {
    /// Start the daemon at sign-in: a per-user Run value (Windows), systemd
    /// user unit (Linux) or LaunchAgent (macOS). Idempotent; does not start it now.
    Enable {
        #[arg(long)]
        prefix: Option<PathBuf>,
    },
    /// Remove the sign-in entry. Idempotent.
    Disable,
    /// Show the current sign-in entry.
    Status,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Install directory (default %LOCALAPPDATA%\WrongMemory\bin on Windows,
    /// $XDG_DATA_HOME/wrongmemory/bin or ~/.local/share/wrongmemory/bin on Unix).
    #[arg(long)]
    pub prefix: Option<PathBuf>,
    /// Stop a running daemon first (otherwise install refuses).
    #[arg(long)]
    pub stop_daemon: bool,
    /// Leave the user PATH alone (also: WRONGMEMORY_NO_MODIFY_PATH=1). By
    /// default the install directory is added to the user PATH (Windows) or
    /// to ~/.profile and the shell's startup file (Linux, macOS).
    #[arg(long)]
    pub no_modify_path: bool,
}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    #[arg(long)]
    pub prefix: Option<PathBuf>,
    /// Also delete the WrongMemory data root (managed stores, registry,
    /// grants, logs). Requires --yes. Adopted project stores are never touched.
    #[arg(long)]
    pub remove_data: bool,
    #[arg(long)]
    pub yes: bool,
}

fn print(json_mode: bool, v: &Value, human: String) {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        println!("{human}");
    }
}

fn exe_name(base: &str) -> String {
    format!("{base}{}", std::env::consts::EXE_SUFFIX)
}

#[cfg(windows)]
pub fn default_prefix() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|p| PathBuf::from(p).join("WrongMemory").join("bin"))
}

/// `$XDG_DATA_HOME/wrongmemory/bin`, else `~/.local/share/wrongmemory/bin`.
#[cfg(unix)]
pub fn default_prefix() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local").join("share"))
        })?;
    Some(data.join("wrongmemory").join("bin"))
}

fn resolve_prefix(p: Option<&Path>) -> Result<PathBuf, String> {
    match p {
        Some(p) => std::path::absolute(p).map_err(|e| e.to_string()),
        None => default_prefix().ok_or_else(|| {
            if cfg!(windows) {
                "cannot determine %LOCALAPPDATA%; pass --prefix".into()
            } else {
                "cannot determine $HOME; pass --prefix".into()
            }
        }),
    }
}

/// `path` with `.tag` appended: `wrongmemory.exe.old` on Windows,
/// `wrongmemory.old` on Unix.
fn tagged(path: &Path, tag: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(tag);
    PathBuf::from(s)
}

// ─── backup ────────────────────────────────────────────────────────────────

pub async fn run_backup(root: &DataRoot, json_mode: bool, a: BackupArgs) -> i32 {
    let store = match (&a.store, &a.project) {
        (Some(s), _) => s.clone(),
        (None, Some(p)) => match Client::connect_host(root, "wrongmemory-backup").await {
            Ok(mut c) => match c
                .call_raw(method::PROJECT_GET, Some(p), json!({}), None)
                .await
            {
                Ok(v) => PathBuf::from(v["storePath"].as_str().unwrap_or_default()),
                Err(e) => {
                    eprintln!("wrongmemory backup: {e}");
                    return e.code().map(|c| c.exit_code()).unwrap_or(exit_code::FAILED);
                }
            },
            Err(e) => {
                eprintln!(
                    "wrongmemory backup: daemon not reachable ({e}); pass --store <path> to back up offline"
                );
                return exit_code::SERVICE_UNAVAILABLE;
            }
        },
        (None, None) => {
            eprintln!("wrongmemory backup: give a project id or --store");
            return exit_code::INVALID_INPUT;
        }
    };
    match wrongmemory_storage::backup::backup_store(&store, &a.output) {
        Ok(r) => {
            let v = serde_json::to_value(&r).unwrap_or(Value::Null);
            print(
                json_mode,
                &v,
                format!(
                    "backup written: {} ({} bytes, {} pages, integrity {}, {} memories)\n  sha256 {}\n  NOTE: backups are plain SQLite and not encrypted; protect them like the source.",
                    r.destination,
                    r.bytes,
                    r.pages,
                    r.integrity,
                    r.memory_rows.unwrap_or(0),
                    r.sha256
                ),
            );
            exit_code::SUCCESS
        }
        Err(e) => {
            eprintln!("wrongmemory backup: {e}");
            e.code.exit_code()
        }
    }
}

// ─── migrate inspect ───────────────────────────────────────────────────────

pub fn migration_plan(insp: &wrongmemory_storage::Inspection) -> Vec<String> {
    let mut plan = Vec::new();
    match insp.verdict {
        Verdict::Compatible => {
            plan.push("Readable and writable by this build (SAGE SQLite v5).".into());
            plan.push("Read-only adoption: `wrongmemory project add <root>` (no bytes change).".into());
            if insp.wrongmemory_extension.is_none() {
                plan.push("Write adoption (`--write`) applies the additive extension wrongmemory-ext-v1 (tables wm_profile_extensions, wm_mutation_receipts) in one transaction; SAGE readers ignore them.".into());
            } else {
                plan.push("WrongMemory extension already present.".into());
            }
            if insp.legacy_owner_metadata {
                plan.push("server.json present: a WrongStack SAGE daemon may own this store. Write adoption stays read-only until it is stopped and `--force-owner` is given.".into());
            }
            plan.push("Take a backup first: `wrongmemory backup --store <path> --output <file>`.".into());
        }
        Verdict::UnknownData => plan.push(format!(
            "Readable only: {}. Write adoption is refused until this build understands the extra data.",
            insp.detail
        )),
        Verdict::Older => plan.push(
            "Older SAGE schema. WrongMemory does not migrate SAGE schemas: open the project once with the WrongStack version that owns it so SAGE migrates to v5, then inspect again.".into(),
        ),
        Verdict::Newer => plan.push(
            "Newer schema than this build supports (PROFILE_UNSUPPORTED). Upgrade WrongMemory; never downgrade the store.".into(),
        ),
        Verdict::Divergent => plan.push(format!(
            "Divergent store ({}). Refused; inspect with SAGE doctor before any adoption.",
            insp.detail
        )),
        Verdict::NotASageStore => plan.push("Not a SAGE store. Nothing to adopt.".into()),
        Verdict::Unreadable => plan.push(format!("Unreadable: {}", insp.detail)),
        Verdict::Missing => plan.push("No store at this path. For a new project use `wrongmemory project add <root> --managed`.".into()),
    }
    plan
}

pub fn run_migrate(json_mode: bool, cmd: MigrateCmd) -> i32 {
    let MigrateCmd::Inspect { store } = cmd;
    let insp = wrongmemory_storage::inspect(&store);
    let plan = migration_plan(&insp);
    let v = json!({"inspection": insp, "plan": plan});
    print(
        json_mode,
        &v,
        format!(
            "{} — {:?}: {}\n{}",
            store.display(),
            insp.verdict,
            insp.detail,
            plan.iter()
                .map(|l| format!("  - {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    );
    if insp.verdict.readable() {
        exit_code::SUCCESS
    } else {
        exit_code::UNSUPPORTED
    }
}

// ─── diagnostics ───────────────────────────────────────────────────────────

/// Redact anything that could be a credential: long hex/base64 runs and
/// SAGE secret patterns. Log lines carry no memory text by design.
pub fn redact_line(line: &str) -> String {
    if wrongmemory_storage::rules::looks_like_secret(line) {
        return "[redacted: secret-like content]".into();
    }
    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 32 {
            out.push_str("[redacted]");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in line.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '+' || c == '/' || c == '=' {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

pub async fn run_diagnostics(root: &DataRoot, json_mode: bool, cmd: DiagnosticsCmd) -> i32 {
    let DiagnosticsCmd::Export { output } = cmd;
    if output.exists()
        && std::fs::read_dir(&output)
            .map(|mut d| d.next().is_some())
            .unwrap_or(true)
    {
        eprintln!(
            "wrongmemory diagnostics: {} exists and is not empty",
            output.display()
        );
        return exit_code::INVALID_INPUT;
    }
    if let Err(e) = std::fs::create_dir_all(&output) {
        eprintln!("wrongmemory diagnostics: {e}");
        return exit_code::FAILED;
    }
    let mut files = Vec::new();
    let mut write = |name: &str, v: &Value| {
        let p = output.join(name);
        if std::fs::write(&p, serde_json::to_vec_pretty(v).unwrap_or_default()).is_ok() {
            files.push(name.to_string());
        }
    };
    write(
        "manifest.json",
        &json!({
            "cliVersion": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "sqlite": wrongmemory_storage::sqlite_version(),
            "dataRoot": root.path().display().to_string(),
            "storageProfile": wrongmemory_contracts::STORAGE_PROFILE_ID,
            "semanticProfile": wrongmemory_contracts::SEMANTIC_PROFILE_ID,
            "contents": "versions, daemon health, project/admission list, store inspection counts, redacted log tail; no memory text, no credentials",
        }),
    );
    match Client::connect_host(root, "wrongmemory-diagnostics").await {
        Ok(mut c) => {
            let health = c
                .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
                .await
                .unwrap_or(Value::Null);
            write("health.json", &health);
            let projects = c
                .call_raw(method::PROJECT_LIST, None, json!({}), None)
                .await
                .unwrap_or(Value::Null);
            let mut list = Vec::new();
            let mut inspections = Vec::new();
            for p in projects["projects"].as_array().cloned().unwrap_or_default() {
                list.push(json!({
                    "projectId": p["projectId"], "storeKind": p["storeKind"],
                    "admission": p["admission"], "admissionReason": p["admissionReason"],
                    "ownerEpoch": p["ownerEpoch"], "storagePath": p["storePath"],
                }));
                let insp =
                    wrongmemory_storage::inspect(Path::new(p["storePath"].as_str().unwrap_or("")));
                inspections.push(json!({
                    "projectId": p["projectId"], "verdict": insp.verdict, "detail": insp.detail,
                    "memories": insp.memory_rows, "candidates": insp.candidate_rows, "edges": insp.edge_rows,
                    "schemaMeta": insp.schema_meta, "extension": insp.wrongmemory_extension,
                    "legacyOwnerMetadata": insp.legacy_owner_metadata,
                }));
            }
            write("projects.json", &json!(list));
            write("inspections.json", &json!(inspections));
            let grants = c
                .call_raw(method::GRANT_LIST, None, json!({}), None)
                .await
                .unwrap_or(Value::Null);
            let summary: Vec<Value> = grants["grants"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|g| json!({"grantId": g["grantId"], "profile": g["profile"], "projects": g["projects"], "revoked": !g["revokedAtMs"].is_null()}))
                .collect();
            write("grants.json", &json!(summary));
        }
        Err(e) => write("health.json", &json!({"unavailable": e.to_string()})),
    }
    let log = root.logs_dir().join("wrongmemoryd.log");
    if let Ok(text) = std::fs::read_to_string(&log) {
        let lines: Vec<&str> = text.lines().collect();
        let tail = &lines[lines.len().saturating_sub(MAX_LOG_LINES)..];
        let redacted: Vec<String> = tail.iter().map(|l| redact_line(l)).collect();
        if std::fs::write(output.join("daemon-log-tail.txt"), redacted.join("\n")).is_ok() {
            files.push("daemon-log-tail.txt".into());
        }
    }
    let v = json!({"output": output.display().to_string(), "files": files});
    print(
        json_mode,
        &v,
        format!(
            "diagnostics written to {}: {}",
            output.display(),
            files.join(", ")
        ),
    );
    exit_code::SUCCESS
}

// ─── autostart ─────────────────────────────────────────────────────────────

#[cfg(windows)]
fn autostart_key() -> String {
    std::env::var(AUTOSTART_KEY_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty())
        .unwrap_or_else(|| AUTOSTART_KEY.to_string())
}

/// Current Run value data, if any.
#[cfg(windows)]
pub fn autostart_query() -> Option<String> {
    let out = Command::new("reg")
        .args(["query", &autostart_key(), "/v", AUTOSTART_VALUE])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find(|l| l.trim_start().starts_with(AUTOSTART_VALUE))
        .and_then(|l| l.split_once("REG_SZ").map(|(_, d)| d.trim().to_string()))
}

#[cfg(windows)]
fn autostart_command(root: &DataRoot, daemon: &Path) -> String {
    let mut cmd = format!("\"{}\"", daemon.display());
    let default = DataRoot::resolve(None).ok();
    if default.as_ref() != Some(root) {
        cmd.push_str(&format!(" --data-root \"{}\"", root.path().display()));
    }
    cmd
}

/// The daemon's sign-in arguments: the data root is passed only when it is
/// not the default one.
#[cfg(unix)]
fn autostart_args(root: &DataRoot, daemon: &Path) -> Vec<String> {
    let mut args = vec![daemon.display().to_string()];
    if DataRoot::resolve(None).ok().as_ref() != Some(root) {
        args.push("--data-root".into());
        args.push(root.path().display().to_string());
    }
    args
}

/// The daemon `autostart enable` points at: under `--prefix`, else the
/// installed one, else the one next to this executable.
fn autostart_daemon(prefix: Option<PathBuf>) -> Result<PathBuf, i32> {
    let daemon = match prefix {
        Some(p) => std::path::absolute(p)
            .unwrap_or_default()
            .join(exe_name("wrongmemoryd")),
        None => {
            let installed = default_prefix().map(|p| p.join(exe_name("wrongmemoryd")));
            match installed.filter(|p| p.exists()) {
                Some(p) => p,
                None => match crate::spawn::daemon_exe() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("wrongmemory autostart: {e}");
                        return Err(exit_code::FAILED);
                    }
                },
            }
        }
    };
    if !daemon.exists() {
        eprintln!("wrongmemory autostart: {} does not exist", daemon.display());
        return Err(exit_code::INVALID_INPUT);
    }
    Ok(daemon)
}

#[cfg(unix)]
pub fn run_autostart(root: &DataRoot, json_mode: bool, cmd: AutostartCmd) -> i32 {
    use crate::autostart_unix as unit;
    let file = match unit::file() {
        Ok(f) => f.display().to_string(),
        Err(e) => {
            eprintln!("wrongmemory autostart: {e}");
            return exit_code::FAILED;
        }
    };
    match cmd {
        AutostartCmd::Status => {
            let cur = unit::query().map(|a| unit::display(&a));
            print(
                json_mode,
                &json!({"file": file, "command": cur}),
                match &cur {
                    Some(c) => format!("enabled: {c}\n  {file}"),
                    None => "disabled".into(),
                },
            );
            exit_code::SUCCESS
        }
        AutostartCmd::Enable { prefix } => {
            let daemon = match autostart_daemon(prefix) {
                Ok(d) => d,
                Err(code) => return code,
            };
            let args = autostart_args(root, &daemon);
            let command = unit::display(&args);
            match unit::enable(&args) {
                Ok(changed) => {
                    print(
                        json_mode,
                        &json!({"changed": changed, "command": command, "file": file}),
                        if changed {
                            format!("enabled: {command}\n  {file}")
                        } else {
                            format!("already enabled: {command}")
                        },
                    );
                    exit_code::SUCCESS
                }
                Err(e) => {
                    eprintln!("wrongmemory autostart: {e}");
                    exit_code::FAILED
                }
            }
        }
        AutostartCmd::Disable => match unit::disable() {
            Ok(changed) => {
                print(
                    json_mode,
                    &json!({"changed": changed, "file": file}),
                    if changed {
                        "disabled".into()
                    } else {
                        "already disabled".into()
                    },
                );
                exit_code::SUCCESS
            }
            Err(e) => {
                eprintln!("wrongmemory autostart: {e}");
                exit_code::FAILED
            }
        },
    }
}

#[cfg(windows)]
pub fn run_autostart(root: &DataRoot, json_mode: bool, cmd: AutostartCmd) -> i32 {
    match cmd {
        AutostartCmd::Status => {
            let cur = autostart_query();
            let v = json!({"key": autostart_key(), "value": AUTOSTART_VALUE, "command": cur});
            print(
                json_mode,
                &v,
                match &cur {
                    Some(c) => format!("enabled: {c}"),
                    None => "disabled".into(),
                },
            );
            exit_code::SUCCESS
        }
        AutostartCmd::Enable { prefix } => {
            let daemon = match autostart_daemon(prefix) {
                Ok(d) => d,
                Err(code) => return code,
            };
            let data = autostart_command(root, &daemon);
            if autostart_query().as_deref() == Some(data.as_str()) {
                print(
                    json_mode,
                    &json!({"changed": false, "command": data}),
                    format!("already enabled: {data}"),
                );
                return exit_code::SUCCESS;
            }
            let ok = Command::new("reg")
                .args([
                    "add",
                    &autostart_key(),
                    "/v",
                    AUTOSTART_VALUE,
                    "/t",
                    "REG_SZ",
                    "/d",
                    &data,
                    "/f",
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !ok {
                eprintln!("wrongmemory autostart: reg add failed");
                return exit_code::FAILED;
            }
            print(
                json_mode,
                &json!({"changed": true, "command": data}),
                format!("enabled: {data}"),
            );
            exit_code::SUCCESS
        }
        AutostartCmd::Disable => {
            let changed = autostart_query().is_some()
                && Command::new("reg")
                    .args(["delete", &autostart_key(), "/v", AUTOSTART_VALUE, "/f"])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
            print(
                json_mode,
                &json!({"changed": changed}),
                if changed {
                    "disabled".into()
                } else {
                    "already disabled".into()
                },
            );
            exit_code::SUCCESS
        }
    }
}

// ─── install / uninstall ───────────────────────────────────────────────────

/// Stop a running daemon for `root` and wait until its endpoint is gone.
async fn stop_daemon(root: &DataRoot) -> Result<bool, String> {
    let Ok(mut c) = Client::connect_host(root, "wrongmemory-install").await else {
        return Ok(false);
    };
    c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await
        .map_err(|e| e.to_string())?;
    drop(c);
    let deadline = Instant::now() + Duration::from_secs(20);
    while root.endpoint_file().exists() || Client::connect_host(root, "probe").await.is_ok() {
        if Instant::now() > deadline {
            return Err("daemon did not stop within 20s".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(true)
}

fn replace_atomically(src: &Path, dst: &Path) -> std::io::Result<()> {
    let new = tagged(dst, "new");
    let old = tagged(dst, "old");
    std::fs::copy(src, &new)?;
    if dst.exists() {
        let _ = std::fs::remove_file(&old);
        std::fs::rename(dst, &old)?;
    }
    if let Err(e) = std::fs::rename(&new, dst) {
        if old.exists() {
            let _ = std::fs::rename(&old, dst);
        }
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    Ok(())
}

pub async fn run_install(root: &DataRoot, json_mode: bool, a: InstallArgs) -> i32 {
    let prefix = match resolve_prefix(a.prefix.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("wrongmemory install: {e}");
            return exit_code::INVALID_INPUT;
        }
    };
    let src_dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        Some(d) => d,
        None => return exit_code::FAILED,
    };
    if std::fs::canonicalize(&src_dir).ok() == std::fs::canonicalize(&prefix).ok() {
        eprintln!(
            "wrongmemory install: already running from {}",
            prefix.display()
        );
        return exit_code::INVALID_INPUT;
    }
    for b in BINARIES {
        if !src_dir.join(exe_name(b)).exists() {
            eprintln!(
                "wrongmemory install: {} missing next to this executable",
                exe_name(b)
            );
            return exit_code::FAILED;
        }
    }
    let running = Client::connect_host(root, "probe").await.is_ok();
    let mut stopped = false;
    if running {
        if !a.stop_daemon {
            eprintln!(
                "wrongmemory install: the daemon is running; rerun with --stop-daemon to drain and stop it first"
            );
            return exit_code::CONFLICT;
        }
        match stop_daemon(root).await {
            Ok(s) => stopped = s,
            Err(e) => {
                eprintln!("wrongmemory install: {e}");
                return exit_code::FAILED;
            }
        }
    }
    if let Err(e) = std::fs::create_dir_all(&prefix) {
        eprintln!("wrongmemory install: {e}");
        return exit_code::FAILED;
    }
    let mut done: Vec<PathBuf> = Vec::new();
    for b in BINARIES {
        let dst = prefix.join(exe_name(b));
        if let Err(e) = replace_atomically(&src_dir.join(exe_name(b)), &dst) {
            // Roll back the binaries already replaced in this run.
            for d in &done {
                let old = tagged(d, "old");
                if old.exists() {
                    let _ = std::fs::remove_file(d);
                    let _ = std::fs::rename(&old, d);
                }
            }
            eprintln!(
                "wrongmemory install: replacing {} failed: {e}; rolled back",
                dst.display()
            );
            return exit_code::FAILED;
        }
        done.push(dst);
    }
    let manifest = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "installedFrom": src_dir.display().to_string(),
        "binaries": done.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "rollback": format!(
            "previous binaries (if any) are kept as *{}.old",
            std::env::consts::EXE_SUFFIX
        ),
    });
    let _ = std::fs::write(
        prefix.join("install.json"),
        serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    );
    // PATH last: the binaries are in place even if the profile cannot be edited.
    let path = if a.no_modify_path || crate::user_path::disabled_by_env() {
        json!({"modified": false, "skipped": true})
    } else {
        match crate::user_path::add(&prefix) {
            Ok(changed) => json!({"modified": !changed.is_empty(), "changed": changed}),
            Err(e) => {
                eprintln!(
                    "wrongmemory install: could not add {} to PATH: {e}",
                    prefix.display()
                );
                json!({"modified": false, "error": e})
            }
        }
    };
    let v = json!({
        "prefix": prefix.display().to_string(),
        "installed": manifest["binaries"],
        "daemonStopped": stopped,
        "path": path,
    });
    let path_note = if path["modified"] == true {
        format!(
            "\n  added {} to PATH ({}); open a new terminal to use `wrongmemory`",
            prefix.display(),
            path["changed"]
                .as_array()
                .map(|c| c
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        )
    } else {
        String::new()
    };
    print(
        json_mode,
        &v,
        format!(
            "installed {} to {}{}\n  start: \"{}\" daemon start{path_note}",
            env!("CARGO_PKG_VERSION"),
            prefix.display(),
            if stopped {
                " (daemon was stopped; start it again)"
            } else {
                ""
            },
            prefix.join(exe_name("wrongmemory")).display()
        ),
    );
    exit_code::SUCCESS
}

fn looks_like_data_root(p: &Path) -> bool {
    p.join("registry.db").exists() || p.join("runtime").is_dir()
}

pub async fn run_uninstall(root: &DataRoot, json_mode: bool, a: UninstallArgs) -> i32 {
    if a.remove_data && !a.yes {
        eprintln!(
            "wrongmemory uninstall: --remove-data deletes managed memory stores, grants and logs under {}; add --yes to confirm",
            root.path().display()
        );
        return exit_code::INVALID_INPUT;
    }
    let prefix = match resolve_prefix(a.prefix.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("wrongmemory uninstall: {e}");
            return exit_code::INVALID_INPUT;
        }
    };
    let mut removed: Vec<String> = Vec::new();
    match stop_daemon(root).await {
        Ok(true) => removed.push("daemon stopped (ownership released)".into()),
        Ok(false) => {}
        Err(e) => {
            eprintln!("wrongmemory uninstall: {e}");
            return exit_code::FAILED;
        }
    }
    #[cfg(windows)]
    if let Some(cmd) = autostart_query()
        && cmd
            .to_lowercase()
            .contains(&prefix.display().to_string().to_lowercase())
        && Command::new("reg")
            .args(["delete", &autostart_key(), "/v", AUTOSTART_VALUE, "/f"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        removed.push(format!("autostart value {AUTOSTART_VALUE}"));
    }
    // Only an entry that starts a daemon from this prefix.
    #[cfg(unix)]
    if let Some(args) = crate::autostart_unix::query()
        && args
            .first()
            .is_some_and(|d| Path::new(d).parent() == Some(prefix.as_path()))
        && crate::autostart_unix::disable().unwrap_or(false)
    {
        removed.push(format!(
            "autostart entry {}",
            crate::autostart_unix::file()
                .map(|f| f.display().to_string())
                .unwrap_or_default()
        ));
    }
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    for b in BINARIES {
        let base = prefix.join(exe_name(b));
        for p in [base.clone(), tagged(&base, "old"), tagged(&base, "new")] {
            if !p.exists() {
                continue;
            }
            // A running executable cannot delete itself on Windows: move it
            // aside and leave a note instead of failing. Unix unlinks it.
            if cfg!(windows) && std::fs::canonicalize(&p).ok() == me {
                let aside = tagged(&p, "delete-me");
                if std::fs::rename(&p, &aside).is_ok() {
                    removed.push(format!("{} (renamed; delete after exit)", p.display()));
                }
                continue;
            }
            if std::fs::remove_file(&p).is_ok() {
                removed.push(p.display().to_string());
            }
        }
    }
    let manifest = prefix.join("install.json");
    if manifest.exists() && std::fs::remove_file(&manifest).is_ok() {
        removed.push(manifest.display().to_string());
    }
    if !crate::user_path::disabled_by_env() {
        match crate::user_path::remove(&prefix) {
            Ok(changed) => {
                removed.extend(changed.into_iter().map(|c| format!("PATH entry in {c}")))
            }
            Err(e) => eprintln!("wrongmemory uninstall: could not update PATH: {e}"),
        }
    }
    let _ = std::fs::remove_dir(&prefix);
    if a.remove_data {
        let dr = root.path();
        if !looks_like_data_root(dr) {
            eprintln!(
                "wrongmemory uninstall: {} does not look like a WrongMemory data root; not deleting it",
                dr.display()
            );
            return exit_code::INVALID_INPUT;
        }
        // Only the data root itself; adopted project stores live in project
        // trees and are never under it.
        match std::fs::remove_dir_all(dr) {
            Ok(()) => removed.push(format!("data root {}", dr.display())),
            Err(e) => {
                eprintln!("wrongmemory uninstall: removing data root failed: {e}");
                return exit_code::FAILED;
            }
        }
    }
    let v = json!({"removed": removed, "dataKept": !a.remove_data});
    print(
        json_mode,
        &v,
        format!(
            "removed:\n{}\n{}",
            removed
                .iter()
                .map(|r| format!("  - {r}"))
                .collect::<Vec<_>>()
                .join("\n"),
            if a.remove_data {
                "data root removed".to_string()
            } else {
                format!("data kept at {}", root.path().display())
            }
        ),
    );
    exit_code::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction() {
        assert_eq!(redact_line("token abc"), "token abc");
        assert!(redact_line(&format!("x {} y", "a".repeat(40))).contains("[redacted]"));
        assert_eq!(
            redact_line(&["password=", "hunter2hunter2hunter2"].concat()),
            "[redacted: secret-like content]"
        );
    }
}
