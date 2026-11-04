//! Per-user start at sign-in on Unix (WM-802).
//!
//! - **Linux:** a systemd user unit, `wrongmemory.service`, linked into
//!   `default.target.wants/`. That link is exactly what `systemctl --user
//!   enable` creates, so the daemon starts at the next sign-in even when no
//!   user manager is reachable right now (containers, WSL without systemd).
//!   When one is, `systemctl --user daemon-reload` is run so that `systemctl
//!   --user status wrongmemory` sees the change at once.
//! - **macOS:** a LaunchAgent, `com.wrongstack.wrongmemory.plist`, with
//!   `RunAtLoad`. launchd loads it at the next sign-in.
//!
//! Like the Windows Run value, enabling never starts the daemon now; the CLI
//! and bridges start it on demand. `WRONGMEMORY_AUTOSTART_DIR_OVERRIDE`
//! redirects the unit or agent directory and skips `systemctl`/`launchctl`,
//! so tests never touch the real session.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const DIR_ENV: &str = "WRONGMEMORY_AUTOSTART_DIR_OVERRIDE";

#[cfg(target_os = "macos")]
const FILE_NAME: &str = "com.wrongstack.wrongmemory.plist";
#[cfg(target_os = "macos")]
const LABEL: &str = "com.wrongstack.wrongmemory";
#[cfg(not(target_os = "macos"))]
const FILE_NAME: &str = "wrongmemory.service";

fn overridden() -> Option<PathBuf> {
    std::env::var_os(DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "cannot determine $HOME".to_string())
}

/// Directory holding the unit (Linux) or agent (macOS).
pub fn dir() -> Result<PathBuf, String> {
    if let Some(d) = overridden() {
        return Ok(d);
    }
    #[cfg(target_os = "macos")]
    {
        Ok(home()?.join("Library").join("LaunchAgents"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        Ok(match config {
            Some(c) => c,
            None => home()?.join(".config"),
        }
        .join("systemd")
        .join("user"))
    }
}

pub fn file() -> Result<PathBuf, String> {
    Ok(dir()?.join(FILE_NAME))
}

#[cfg(not(target_os = "macos"))]
fn wants_link(dir: &Path) -> PathBuf {
    dir.join("default.target.wants").join(FILE_NAME)
}

/// Shell-like display of the daemon command, for status output.
pub fn display(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
                format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── Linux: systemd user unit ──────────────────────────────────────────────

/// One `ExecStart=` word: double-quoted, with systemd's C-style escapes and
/// specifier (`%`) and variable (`$`) expansion disabled.
#[cfg(not(target_os = "macos"))]
fn unit_word(a: &str) -> String {
    let mut out = String::from("\"");
    for c in a.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(not(target_os = "macos"))]
fn unit_unword(w: &str) -> String {
    let inner = w
        .strip_prefix('"')
        .and_then(|w| w.strip_suffix('"'))
        .unwrap_or(w);
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '%' | '$' if chars.peek() == Some(&c) => {
                chars.next();
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(not(target_os = "macos"))]
fn render(args: &[String]) -> String {
    let exec = args
        .iter()
        .map(|a| unit_word(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "# Written by `wrongmemory autostart enable`; removed by `autostart disable`.\n\
         [Unit]\n\
         Description=WrongMemory per-user memory service\n\
         Documentation=https://github.com/WrongStack/wrongmemory\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         # 3: another daemon already serves this data root.\n\
         SuccessExitStatus=3\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Split an `ExecStart=` value written by `render` back into arguments.
#[cfg(not(target_os = "macos"))]
fn parse(text: &str) -> Option<Vec<String>> {
    let line = text.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => {
                cur.push(c);
                escaped = true;
            }
            '"' => {
                cur.push(c);
                quoted = !quoted;
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    words.push(unit_unword(&cur));
                    cur.clear();
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(unit_unword(&cur));
    }
    Some(words)
}

#[cfg(not(target_os = "macos"))]
fn enabled_args(dir: &Path) -> Option<Vec<String>> {
    if std::fs::symlink_metadata(wants_link(dir)).is_err() {
        return None;
    }
    parse(&std::fs::read_to_string(dir.join(FILE_NAME)).ok()?)
}

#[cfg(not(target_os = "macos"))]
fn reload_manager() {
    if overridden().is_none() {
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
    }
}

// ─── macOS: LaunchAgent ────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(target_os = "macos")]
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

#[cfg(target_os = "macos")]
fn render(args: &[String]) -> String {
    let program = args
        .iter()
        .map(|a| format!("\t\t<string>{}</string>\n", xml_escape(a)))
        .collect::<String>();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by `wrongmemory autostart enable`; removed by `autostart disable`. -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         {program}\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>ProcessType</key>\n\
         \t<string>Background</string>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// Read back the `ProgramArguments` that `render` wrote.
#[cfg(target_os = "macos")]
fn parse(text: &str) -> Option<Vec<String>> {
    let start = text.find("<key>ProgramArguments</key>")?;
    let rest = &text[start..];
    let array = &rest[rest.find("<array>")? + "<array>".len()..rest.find("</array>")?];
    Some(
        array
            .split("<string>")
            .skip(1)
            .filter_map(|s| s.split_once("</string>").map(|(v, _)| xml_unescape(v)))
            .collect(),
    )
}

#[cfg(target_os = "macos")]
fn enabled_args(dir: &Path) -> Option<Vec<String>> {
    parse(&std::fs::read_to_string(dir.join(FILE_NAME)).ok()?)
}

#[cfg(target_os = "macos")]
fn reload_manager() {}

// ─── shared ────────────────────────────────────────────────────────────────

/// Arguments of the enabled entry, if any.
pub fn query() -> Option<Vec<String>> {
    enabled_args(&dir().ok()?)
}

/// Write (or rewrite) the entry. Returns whether anything changed.
pub fn enable(args: &[String]) -> Result<bool, String> {
    let dir = dir()?;
    let text = render(args);
    let path = dir.join(FILE_NAME);
    let current = std::fs::read_to_string(&path).ok();
    #[cfg(not(target_os = "macos"))]
    let linked = std::fs::symlink_metadata(wants_link(&dir)).is_ok();
    #[cfg(target_os = "macos")]
    let linked = true;
    if current.as_deref() == Some(text.as_str()) && linked {
        return Ok(false);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("writing {}: {e}", path.display())
    })?;
    #[cfg(not(target_os = "macos"))]
    if !linked {
        let link = wants_link(&dir);
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        // Relative, as `systemctl enable` writes it.
        std::os::unix::fs::symlink(Path::new("..").join(FILE_NAME), &link)
            .map_err(|e| format!("linking {}: {e}", link.display()))?;
    }
    reload_manager();
    Ok(true)
}

/// Remove the entry. Returns whether anything was there.
pub fn disable() -> Result<bool, String> {
    let dir = dir()?;
    let mut changed = false;
    #[cfg(not(target_os = "macos"))]
    {
        let link = wants_link(&dir);
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link).map_err(|e| format!("removing {}: {e}", link.display()))?;
            changed = true;
        }
    }
    #[cfg(target_os = "macos")]
    if overridden().is_none() && dir.join(FILE_NAME).exists() {
        // Unload it from the running session too; failure means it was not
        // loaded, which is the usual case after `enable`.
        let uid = wrongmemory_ipc::unix::current_uid();
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{LABEL}")])
            .output();
    }
    let path = dir.join(FILE_NAME);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("removing {}: {e}", path.display()))?;
        changed = true;
    }
    if changed {
        reload_manager();
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        vec![
            "/home/u/My Apps/wrongmemoryd".into(),
            "--data-root".into(),
            "/home/u/data 100% $HOME \"q\" \\x".into(),
        ]
    }

    #[test]
    fn rendered_entry_round_trips_awkward_paths() {
        let text = render(&args());
        assert_eq!(parse(&text).unwrap(), args(), "{text}");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unit_escapes_specifiers_and_variables() {
        let text = render(&args());
        assert!(text.contains("100%% $$HOME"), "{text}");
        assert!(text.contains("WantedBy=default.target"));
        assert!(text.contains("SuccessExitStatus=3"));
    }

    #[test]
    fn display_quotes_only_what_needs_it() {
        assert_eq!(
            display(&["/a/b".into(), "--x".into(), "c d".into()]),
            "/a/b --x \"c d\""
        );
    }
}
