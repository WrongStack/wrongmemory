//! Anchor verification (baseline `anchors/verify.ts` +
//! `sqlite-store-verify.ts`): re-check a memory's anchors against the
//! project tree and reconcile `active` ↔ `stale`.
//!
//! Nothing is ever executed (WM-INV: no shell, no regex evaluation):
//! * file/test/git/symbol/directory/package anchors: `stat`, realpath
//!   containment, sha256 `contentHash`, identifier-boundary symbol search;
//! * git anchors: the blob id is computed natively
//!   (`sha1("blob <len>\0" + bytes)`) instead of spawning `git hash-object`.
//!   A mismatch on a file containing `\r` is reported `unknown`, because
//!   the baseline's git may have normalized line endings first;
//! * command anchors: the executable is resolved from the first token
//!   (wrappers and their flags skipped) and probed on the daemon's `PATH`
//!   by `stat` only; shell builtins count as present;
//! * agent anchors: the fleet roster or `.wrongstack/agents/<role>/`.
//!
//! Reconciliation follows the baseline: a stale verdict flips `active` to
//! `stale` (`staleReason: verification`), a verified verdict reactivates a
//! non-manual stale memory with `freshness: 1`, and a confirmation without a
//! status change only stamps `lastVerifiedAt` (no revision bump). A row whose
//! anchors changed since it was read is left alone.
//!
//! Checkout evidence (WM-INV-14): linked worktrees share one memory store but
//! not their files. A caller bound to a linked worktree gets its anchors
//! checked against that worktree, each result labeled with the checkout, and
//! nothing is reconciled: a file missing on one branch is not evidence that
//! the shared memory is stale.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use wrongmemory_contracts::{MemoryAnchor, SageRecord};

use crate::hygiene::{anchors_equal, semantic};
use crate::mutate::{read_raw, sync_edges};
use crate::paths::normalize_project_path;
use crate::store::{MutationCtx, SageStore, StoreResult, Viewer, decode_memory, sql_err};

/// `FLEET_ROSTER` keys (`@wrongstack/coordination` fleet.ts at the baseline).
const FLEET_ROSTER: &[&str] = &[
    "audit-log",
    "bug-hunter",
    "refactor-planner",
    "security-scanner",
    "critic",
    "generic",
    "shadow-agent",
    "explore-companion",
    "chaos-monkey",
];

const COMMAND_WRAPPERS: &[&str] = &["sudo", "npx", "env", "time", "command", "doas", "runuser"];
const DEMAND_FETCH_WRAPPERS: &[&str] = &["npx"];

fn wrapper_value_flags(wrapper: &str) -> &'static [&'static str] {
    match wrapper {
        "npx" => &[
            "-p",
            "--package",
            "--registry",
            "--cache",
            "--userconfig",
            "--globalconfig",
            "--prefix",
            "--cwd",
            "--shell",
        ],
        "sudo" => &[
            "-u",
            "-g",
            "-p",
            "-C",
            "-R",
            "-D",
            "-T",
            "--user",
            "--group",
            "--prompt",
            "--chdir",
            "--command-timeout",
        ],
        "env" => &["-u", "-C", "--unset", "--chdir"],
        "doas" => &["-u"],
        "runuser" => &["-u", "-g"],
        _ => &[],
    }
}

const BASH_BUILTINS: &[&str] = &[
    "cd", "echo", "type", "alias", "exit", "set", "unset", "printf", "pwd", "export", "test",
    "true", "false", "read", "shift", "source", ".",
];
const CMD_BUILTINS: &[&str] = &[
    "dir", "copy", "del", "ren", "mkdir", "md", "rd", "rmdir", "move", "erase", "call", "pushd",
    "popd", "cls", "if", "for", "goto", "rem", "pause", "color", "chcp", "setlocal", "endlocal",
    "ver", "vol", "path", "prompt", "title", "break", "assoc", "ftype",
];

fn is_shell_builtin(exe: &str) -> bool {
    if cfg!(windows) {
        let k = exe.to_lowercase();
        BASH_BUILTINS.contains(&k.as_str()) || CMD_BUILTINS.contains(&k.as_str())
    } else {
        BASH_BUILTINS.contains(&exe)
    }
}

/// `firstTokenInfo`: a quoted first token, or the first non-space run.
/// Returns (token, rest-after-token).
fn first_token(input: &str) -> Option<(String, &str)> {
    let s = input;
    for q in ['"', '\''] {
        if let Some(body) = s.strip_prefix(q)
            && let Some(end) = body.find(q)
            && end > 0
        {
            let tok = body[..end].trim().to_string();
            return (!tok.is_empty()).then(|| (tok, &body[end + 1..]));
        }
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((s[..end].to_string(), &s[end..]))
}

struct Resolved {
    executable: Option<String>,
    skipped_flag: bool,
    demand_fetch: bool,
    wrapper: Option<String>,
}

fn is_env_assignment(t: &str) -> bool {
    let Some((name, _)) = t.split_once('=') else {
        return false;
    };
    let mut cs = name.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `resolveCommandExecutable`.
fn resolve_command(command: &str) -> Resolved {
    let none = |skipped, demand, wrapper| Resolved {
        executable: None,
        skipped_flag: skipped,
        demand_fetch: demand,
        wrapper,
    };
    let trimmed = command.trim();
    let Some((first, rest)) = first_token(trimmed) else {
        return none(false, false, None);
    };
    if !COMMAND_WRAPPERS.contains(&first.as_str()) {
        return Resolved {
            executable: Some(first),
            skipped_flag: false,
            demand_fetch: false,
            wrapper: None,
        };
    }
    let demand = DEMAND_FETCH_WRAPPERS.contains(&first.as_str());
    let value_flags = wrapper_value_flags(&first);
    let mut cursor = rest.trim();
    let mut skipped = false;
    for _ in 0..8 {
        let Some((tok, after)) = first_token(cursor) else {
            return none(skipped, demand, Some(first));
        };
        if is_env_assignment(&tok) {
            cursor = after.trim();
            continue;
        }
        if tok.starts_with('-') {
            cursor = after.trim();
            if value_flags.contains(&tok.as_str()) {
                if let Some((_, after_val)) = first_token(cursor) {
                    cursor = after_val.trim();
                }
            } else {
                skipped = true;
            }
            continue;
        }
        return Resolved {
            executable: Some(tok),
            skipped_flag: skipped,
            demand_fetch: demand,
            wrapper: Some(first),
        };
    }
    none(skipped, demand, Some(first))
}

fn is_executable_file(meta: &std::fs::Metadata) -> bool {
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `commandExists`, restricted: `Some(found)`, or `None` when the anchor
/// names a location the daemon must not touch.
///
/// Explicit paths are only probed when they resolve inside the project root;
/// UNC, verbatim and device paths and absolute paths elsewhere are never
/// stat'ed (a stored anchor must not make the daemon open a network share or
/// act as an existence oracle for arbitrary files). Bare names are probed on
/// the daemon's own `PATH`.
fn command_exists(root: &Path, real_root: &Path, exe: &str) -> Option<bool> {
    if exe.contains('/') || exe.contains('\\') {
        if exe.starts_with("\\\\") || exe.starts_with("//") {
            return None;
        }
        let p = Path::new(exe);
        // A leading backslash is a rooted Windows path even where the OS
        // reads it as a file name: anchors travel between machines (HQ sync),
        // so a Unix daemon must not report one as a missing project file.
        if p.is_absolute() || p.has_root() || exe.starts_with('\\') || exe.contains(':') {
            return None;
        }
        let target = crate::wrongstack_paths::node_resolve(&root.join(p));
        if !same_or_inside(&crate::wrongstack_paths::node_resolve(root), &target) {
            return None;
        }
        let Ok(real) = std::fs::canonicalize(&target).map(strip_verbatim) else {
            return Some(false);
        };
        if !same_or_inside(real_root, &real) {
            return None;
        }
        return Some(std::fs::metadata(real).is_ok_and(|m| is_executable_file(&m)));
    }
    if exe.contains(':') || exe == "." || exe == ".." {
        return None;
    }
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ".com", ""]
    } else {
        &[""]
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let dir = if cfg!(windows) {
            PathBuf::from(dir.to_string_lossy().trim().trim_matches('"'))
        } else {
            dir
        };
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in exts {
            if std::fs::metadata(dir.join(format!("{exe}{ext}")))
                .is_ok_and(|m| is_executable_file(&m))
            {
                return Some(true);
            }
        }
    }
    Some(false)
}

fn result(anchor: &Value, status: &str, reason: String) -> Value {
    json!({"anchor": anchor, "status": status, "reason": reason})
}

fn command_anchor(root: &Path, real_root: &Path, a: &MemoryAnchor, raw: &Value) -> Value {
    let r = resolve_command(a.command.as_deref().unwrap_or(""));
    let Some(exe) = r.executable else {
        return result(
            raw,
            "stale",
            "Command anchor has no executable to resolve.".into(),
        );
    };
    if is_shell_builtin(&exe) {
        return result(
            raw,
            "verified",
            format!("Command \"{exe}\" is a shell builtin (provided by the shell)."),
        );
    }
    let Some(found) = command_exists(root, real_root, &exe) else {
        return result(
            raw,
            "unknown",
            format!("Command executable \"{exe}\" is outside the project root; not probed."),
        );
    };
    if found {
        return result(
            raw,
            "verified",
            format!("Command executable \"{exe}\" is available."),
        );
    }
    if r.skipped_flag {
        return result(
            raw,
            "unknown",
            format!(
                "Ambiguous command anchor: \"{exe}\" follows a flag (likely a flag argument); not found on PATH."
            ),
        );
    }
    if r.demand_fetch {
        return result(
            raw,
            "unknown",
            format!(
                "Command \"{exe}\" is not installed; \"{}\" installs it on demand, so absence is not staleness.",
                r.wrapper.unwrap_or_default()
            ),
        );
    }
    result(
        raw,
        "stale",
        format!("Command executable \"{exe}\" was not found on PATH."),
    )
}

/// `assertProjectAgentRole`: `^[a-z0-9][a-z0-9._-]{0,95}$` (case-insensitive),
/// not `.`/`..`, no separators.
fn valid_agent_role(role: &str) -> Option<String> {
    let r = role.trim();
    let ok = !r.is_empty()
        && r.len() <= 96
        && r.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && r != "."
        && r != "..";
    ok.then(|| r.to_string())
}

fn agent_anchor(root: &Path, a: &MemoryAnchor, raw: &Value) -> Value {
    let Some(role) = a
        .role
        .as_deref()
        .and_then(valid_agent_role)
        .map(|r| r.to_lowercase())
    else {
        return result(raw, "stale", "Agent anchor has an invalid role.".into());
    };
    let custom = root.join(".wrongstack").join("agents").join(&role);
    if FLEET_ROSTER.contains(&role.as_str()) || custom.is_dir() {
        result(
            raw,
            "verified",
            format!("Agent role \"{role}\" is available."),
        )
    } else {
        result(
            raw,
            "stale",
            format!("Agent role \"{role}\" is not in the roster."),
        )
    }
}

/// `containsSymbol`: `symbol` delimited by characters outside
/// `[\p{L}\p{N}_$]` on both sides.
fn contains_symbol(text: &str, symbol: &str) -> bool {
    if symbol.is_empty() {
        return true;
    }
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut from = 0;
    while let Some(pos) = text[from..].find(symbol) {
        let start = from + pos;
        let end = start + symbol.len();
        let before_ok = text[..start].chars().next_back().is_none_or(|c| !ident(c));
        let after_ok = text[end..].chars().next().is_none_or(|c| !ident(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + text[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn same_or_inside(root: &Path, target: &Path) -> bool {
    let rc: Vec<_> = root.components().collect();
    let tc: Vec<_> = target.components().collect();
    tc.len() >= rc.len()
        && rc.iter().zip(&tc).all(|(a, b)| {
            if cfg!(windows) {
                a.as_os_str().to_string_lossy().to_lowercase()
                    == b.as_os_str().to_string_lossy().to_lowercase()
            } else {
                a == b
            }
        })
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().into_owned();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

/// `verifyAnchor` for path anchors.
fn path_anchor(root: &Path, real_root: &Path, a: &MemoryAnchor, raw: &Value) -> Value {
    let Some(p) = a.path.as_deref().filter(|p| !p.is_empty()) else {
        return result(raw, "unknown", "Anchor has no path.".into());
    };
    let abs = crate::wrongstack_paths::node_resolve(&root.join(p));
    if !same_or_inside(&crate::wrongstack_paths::node_resolve(root), &abs) {
        return result(
            raw,
            "stale",
            "Anchor resolves outside the project root.".into(),
        );
    }
    let (Ok(meta), Ok(real)) = (std::fs::metadata(&abs), std::fs::canonicalize(&abs)) else {
        return result(raw, "stale", "Anchored path no longer exists.".into());
    };
    let real = strip_verbatim(real);
    if !same_or_inside(real_root, &real) {
        return result(
            raw,
            "stale",
            "Anchor resolves through a symlink outside the project root.".into(),
        );
    }
    if matches!(a.kind.as_str(), "directory" | "package") {
        let pkg = a.kind == "package";
        return if meta.is_dir() {
            result(
                raw,
                "verified",
                if pkg {
                    "Package directory exists."
                } else {
                    "Directory exists."
                }
                .into(),
            )
        } else {
            result(
                raw,
                "stale",
                format!(
                    "{} anchor points to a non-directory.",
                    if pkg { "Package" } else { "Directory" }
                ),
            )
        };
    }
    if !meta.is_file() {
        return result(raw, "stale", "File anchor points to a non-file.".into());
    }
    let Ok(body) = std::fs::read(&real) else {
        return result(raw, "stale", "Anchored path no longer exists.".into());
    };
    let content_hash = format!("sha256:{}", hex::encode(Sha256::digest(&body)));
    let with_hash = |mut v: Value| {
        v["contentHash"] = json!(content_hash);
        v
    };
    let expected_content = a.extra.get("contentHash").and_then(Value::as_str);
    if expected_content.is_some_and(|h| h != content_hash) {
        return with_hash(result(raw, "stale", "File content hash changed.".into()));
    }
    if let Some(sym) = a.symbol.as_deref()
        && !contains_symbol(&String::from_utf8_lossy(&body), sym)
    {
        return with_hash(result(
            raw,
            "stale",
            format!("Symbol \"{sym}\" no longer exists."),
        ));
    }
    let expected_blob = a.extra.get("gitBlobHash").and_then(Value::as_str);
    let mut git_blob = None;
    if expected_blob.is_some() || a.kind == "git" {
        let mut h = Sha1::new();
        h.update(format!("blob {}\0", body.len()).as_bytes());
        h.update(&body);
        let blob = hex::encode(h.finalize());
        if let Some(want) = expected_blob
            && want != blob
        {
            let mut v = if body.contains(&b'\r') {
                result(
                    raw,
                    "unknown",
                    "Git blob differs from the raw file, which has CR line endings git may normalize."
                        .into(),
                )
            } else {
                result(raw, "stale", "Git blob hash changed.".into())
            };
            v["contentHash"] = json!(content_hash);
            v["gitBlobHash"] = json!(blob);
            return v;
        }
        git_blob = Some(blob);
    }
    let mut v = with_hash(result(raw, "verified", "Anchor is current.".into()));
    if let Some(b) = git_blob {
        v["gitBlobHash"] = json!(b);
    }
    v
}

/// `aggregateStatus`.
fn aggregate(results: &[Value]) -> &'static str {
    let st = |s: &str| results.iter().any(|r| r["status"] == s);
    if results.is_empty() {
        "unknown"
    } else if st("contradicted") {
        "contradicted"
    } else if st("stale") {
        "stale"
    } else if results.iter().all(|r| r["status"] == "verified") {
        "verified"
    } else {
        "unknown"
    }
}

/// `verifyMemoryAnchors`.
fn verify_memory(root: &Path, real_root: &Path, raw: &Value, m: &SageRecord, at: &str) -> Value {
    let raw_anchors = raw
        .get("anchors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let anchors: Vec<Value> = m
        .anchors
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let ra = raw_anchors.get(i).cloned().unwrap_or(Value::Null);
            match a.kind.as_str() {
                "command" => command_anchor(root, real_root, a, &ra),
                "agent" => agent_anchor(root, a, &ra),
                _ => path_anchor(root, real_root, a, &ra),
            }
        })
        .collect();
    json!({
        "memoryId": m.id,
        "status": aggregate(&anchors),
        "checkedAt": at,
        "anchors": anchors,
    })
}

struct Update {
    id: String,
    observed_anchors: Vec<MemoryAnchor>,
    observed_status: String,
    status: Option<&'static str>,
    freshness: Option<f64>,
}

impl SageStore {
    /// `verify(memoryId?)`: every visible non-deleted memory, or one.
    /// `checkout` is a linked worktree the caller is bound to: evidence comes
    /// from it and is reported only (see the module notes).
    pub fn verify(
        &mut self,
        memory_id: Option<&str>,
        ctx: &MutationCtx,
        checkout: Option<&Path>,
    ) -> StoreResult<Vec<Value>> {
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        let rows: Vec<String> = match memory_id {
            Some(id) => self
                .get(id, &viewer)?
                .filter(|m| m.status != "deleted")
                .and_then(|_| read_raw(&self.conn, id).ok().flatten())
                .map(|(m, _)| vec![Value::Object(m).to_string()])
                .unwrap_or_default(),
            None => {
                let clause = crate::store::session_clause("", &viewer, 1);
                self.conn
                    .prepare(&format!(
                        "SELECT data FROM memories WHERE status != 'deleted' AND {clause}"
                    ))
                    .map_err(sql_err)?
                    .query_map([&viewer.session_id], |r| r.get::<_, String>(0))
                    .map_err(sql_err)?
                    .filter_map(Result::ok)
                    .collect()
            }
        };
        self.reconcile(rows, ctx, checkout)
    }

    /// `verifyForPaths(paths)`: memories whose file or symbol anchors point
    /// at one of `paths` (paths outside the checked tree are ignored).
    pub fn verify_for_paths(
        &mut self,
        paths: &[String],
        ctx: &MutationCtx,
        checkout: Option<&Path>,
    ) -> StoreResult<Vec<Value>> {
        let Some(root) = checkout
            .map(Path::to_path_buf)
            .or_else(|| self.project_root.clone())
        else {
            return Ok(vec![]);
        };
        let mut rels: Vec<String> = Vec::new();
        for p in paths {
            if let Ok(r) = normalize_project_path(&root, p)
                && r != "."
                && !rels.contains(&r)
            {
                rels.push(r);
            }
        }
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        let mut ids: Vec<String> = Vec::new();
        for rel in &rels {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT from_node FROM edges WHERE to_node = ?1
                     UNION ALL
                     SELECT from_node FROM edges WHERE to_node >= ?2 AND to_node < ?3",
                )
                .map_err(sql_err)?;
            let found: Vec<String> = stmt
                .query_map(
                    [
                        format!("file:{rel}"),
                        format!("symbol:{rel}#"),
                        format!("symbol:{rel}$"),
                    ],
                    |r| r.get::<_, String>(0),
                )
                .map_err(sql_err)?
                .filter_map(Result::ok)
                .collect();
            for n in found {
                if let Some(id) = n.strip_prefix("mem:")
                    && !ids.iter().any(|x| x == id)
                {
                    ids.push(id.to_string());
                }
            }
        }
        let mut rows = Vec::new();
        for id in ids {
            if self
                .get(&id, &viewer)?
                .is_some_and(|m| m.status != "deleted")
                && let Some((m, _)) = read_raw(&self.conn, &id).map_err(sql_err)?
            {
                rows.push(Value::Object(m).to_string());
            }
        }
        self.reconcile(rows, ctx, checkout)
    }

    /// `reconcileVerifiedMemories`. With a linked-worktree `checkout`, only
    /// the labeled results are returned; the shared rows are not touched.
    fn reconcile(
        &mut self,
        rows: Vec<String>,
        ctx: &MutationCtx,
        checkout: Option<&Path>,
    ) -> StoreResult<Vec<Value>> {
        let Some(root) = checkout
            .map(Path::to_path_buf)
            .or_else(|| self.project_root.clone())
        else {
            return Ok(vec![]);
        };
        let real_root = std::fs::canonicalize(&root)
            .map(strip_verbatim)
            .unwrap_or_else(|_| crate::wrongstack_paths::node_resolve(&root));
        let at = ctx.now_iso.clone();
        let mut results = Vec::new();
        let mut updates: Vec<Update> = Vec::new();
        for data in rows {
            let Some(m) = decode_memory(&data) else {
                continue;
            };
            let raw: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
            let mut r = verify_memory(&root, &real_root, &raw, &m, &at);
            if checkout.is_some() {
                r["checkout"] = json!({
                    "root": root.display().to_string(),
                    "kind": wrongmemory_contracts::CHECKOUT_LINKED_WORKTREE,
                });
                results.push(r);
                continue;
            }
            let status = r["status"].as_str().unwrap_or("unknown");
            let base = |status, freshness| Update {
                id: m.id.clone(),
                observed_anchors: m.anchors.clone(),
                observed_status: m.status.clone(),
                status,
                freshness,
            };
            if status == "stale" && m.status == "active" {
                updates.push(base(Some("stale"), None));
            } else if status == "verified" {
                updates.push(match m.status.as_str() {
                    "stale" if m.stale_reason.as_deref() != Some("manual") => {
                        base(Some("active"), Some(1.0))
                    }
                    "stale" => base(None, None),
                    "active" => base(None, Some(1.0)),
                    _ => base(None, None),
                });
            } else if !m.anchors.is_empty() && m.last_verified_at.as_deref() != Some(at.as_str()) {
                updates.push(base(None, None));
            }
            results.push(r);
        }
        if updates.is_empty() {
            return Ok(results);
        }
        self.require_writable()?;
        self.in_transaction(|store| {
            let conn = &store.conn;
            for u in &updates {
                let Some((_, cur)) = read_raw(conn, &u.id).map_err(sql_err)? else {
                    continue;
                };
                if !anchors_equal(&cur.anchors, &u.observed_anchors) {
                    continue;
                }
                if cur.status != u.observed_status
                    && !matches!(cur.status.as_str(), "active" | "stale")
                {
                    continue;
                }
                let next = match (u.status, cur.status.as_str()) {
                    (Some("stale"), "active") => "stale",
                    (Some("active"), "stale") if cur.stale_reason.as_deref() != Some("manual") => {
                        "active"
                    }
                    _ => cur.status.as_str(),
                };
                if next == cur.status {
                    // Confirmation only: bookkeeping, no revision bump.
                    match u.freshness {
                        None => conn.execute(
                            "UPDATE memories SET data = json_set(data, '$.lastVerifiedAt', ?1)
                              WHERE id = ?2 AND json_valid(data)",
                            rusqlite::params![at, u.id],
                        ),
                        Some(f) => conn.execute(
                            "UPDATE memories SET data = json_set(data, '$.lastVerifiedAt', ?1, '$.freshness', ?2), freshness = ?2
                              WHERE id = ?3 AND json_valid(data)",
                            rusqlite::params![at, f, u.id],
                        ),
                    }
                    .map_err(sql_err)?;
                    continue;
                }
                let mut patch: Vec<(&str, Option<Value>)> = vec![
                    ("status", Some(json!(next))),
                    (
                        "staleReason",
                        (next == "stale").then(|| json!("verification")),
                    ),
                    ("lastVerifiedAt", Some(json!(at))),
                ];
                if let Some(f) = u.freshness {
                    patch.push(("freshness", Some(json!(f))));
                }
                if let Some(rec) = semantic(conn, &u.id, patch, &at)? {
                    sync_edges(conn, &rec, &at)?;
                }
            }
            Ok(())
        })?;
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_boundaries_follow_the_baseline_lookarounds() {
        assert!(contains_symbol(
            "export function startServer() {}",
            "startServer"
        ));
        assert!(!contains_symbol("startServerFast()", "startServer"));
        assert!(!contains_symbol("$startServer", "startServer"));
        assert!(contains_symbol("const $ref = 1", "$ref"));
        assert!(contains_symbol("this.#priv = 2", "#priv"));
        assert!(!contains_symbol("ünicodeName", "nicodeName"));
        assert!(contains_symbol(
            "a startServer b startServerX",
            "startServer"
        ));
    }

    #[test]
    fn command_resolution_skips_wrappers_and_their_flags() {
        let r = resolve_command("sudo -u www node server.js");
        assert_eq!(r.executable.as_deref(), Some("node"));
        assert!(!r.skipped_flag);
        let r = resolve_command("env FOO=1 -x bun test");
        assert_eq!(r.executable.as_deref(), Some("bun"));
        assert!(r.skipped_flag);
        let r = resolve_command("npx --package tsx tsx run.ts");
        assert_eq!(r.executable.as_deref(), Some("tsx"));
        assert!(r.demand_fetch);
        let r = resolve_command("\"C:\\Program Files\\node\\node.exe\" --version");
        assert_eq!(
            r.executable.as_deref(),
            Some("C:\\Program Files\\node\\node.exe")
        );
        assert!(resolve_command("   ").executable.is_none());
    }

    #[test]
    fn explicit_command_paths_never_leave_the_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts/build.sh"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join("scripts/build.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        std::fs::write(tmp.path().join("outside.exe"), "x").unwrap();
        let real_root = strip_verbatim(std::fs::canonicalize(&root).unwrap());
        let probe = |exe: &str| command_exists(&root, &real_root, exe);
        assert_eq!(probe("./scripts/build.sh"), Some(true));
        assert_eq!(probe("scripts/missing.sh"), Some(false));
        assert_eq!(probe("../outside.exe"), None);
        assert_eq!(
            probe(&tmp.path().join("outside.exe").to_string_lossy()),
            None
        );
        assert_eq!(probe(r"\\attacker\share\tool.exe"), None);
        assert_eq!(probe("//attacker/share/tool"), None);
        assert_eq!(probe(r"\\?\C:\Windows\notepad.exe"), None);
        assert_eq!(probe(r"\Windows\notepad.exe"), None);
        assert_eq!(probe("C:tool.exe"), None);
        assert_eq!(probe("zz-wrongmemory-missing-tool"), Some(false));
    }

    #[test]
    fn git_blob_id_matches_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        let body = b"hello\n";
        let mut h = Sha1::new();
        h.update(format!("blob {}\0", body.len()).as_bytes());
        h.update(body);
        assert_eq!(
            hex::encode(h.finalize()),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn agent_roles_validate_like_the_baseline() {
        assert_eq!(valid_agent_role(" Critic ").as_deref(), Some("Critic"));
        assert!(valid_agent_role("..").is_none());
        assert!(valid_agent_role("a/b").is_none());
        assert!(valid_agent_role("-lead").is_none());
        assert!(valid_agent_role(&"a".repeat(97)).is_none());
    }
}
