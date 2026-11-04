//! WM-800 (partial): `wrongmemory ui` serves a read-only dashboard on
//! loopback. This drives the real command over raw HTTP and checks its
//! access rules: no access without the run's key, the one-time key traded
//! for a separate `HttpOnly`/`SameSite=Strict` session cookie, foreign `Host` headers refused (DNS
//! rebinding), GET only, read-only method allowlist, and no secret in any
//! response.

#![cfg(any(windows, unix))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

fn wm(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_wrongmemory"));
    c.arg("--data-root")
        .arg(home)
        .env_remove("WRONGMEMORY_FAULT");
    c
}

fn run_json(home: &Path, args: &[&str]) -> Value {
    let out = wm(home).arg("--json").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "wrongmemory {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

struct Ui(Child);

/// Stops the data root's daemon even when an assertion panics.
struct StopDaemon<'a>(&'a Path);

impl Drop for StopDaemon<'_> {
    fn drop(&mut self) {
        let _ = wm(self.0).args(["daemon", "stop"]).output();
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Raw HTTP/1.1 request; returns (status line, headers, body).
fn http(
    port: u16,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
) -> (String, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut req = format!("{method} {target} HTTP/1.1\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let (status, headers) = head.split_once("\r\n").unwrap_or((head, ""));
    (status.to_string(), headers.to_string(), body.to_string())
}

#[test]
fn dashboard_is_loopback_keyed_and_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _stop = StopDaemon(&home);
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let project = run_json(
        &home,
        &["project", "add", proj.to_str().unwrap(), "--managed"],
    )["project"]["projectId"]
        .as_str()
        .unwrap()
        .to_string();
    run_json(
        &home,
        &["remember", &project, "The dashboard lists this memory"],
    );
    let grant = run_json(
        &home,
        &[
            "grants",
            "create",
            "--label",
            "agent",
            "--project",
            &project,
        ],
    );
    let grant_id = grant["grant"]["grantId"].as_str().unwrap().to_string();
    let secret = std::fs::read_to_string(home.join("grants").join(&grant_id)).unwrap_or_default();

    let mut child = wm(&home)
        .args(["ui", "--no-open"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _ui = Ui(child);
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let url = line.split_whitespace().last().unwrap().to_string();
    let rest = url.strip_prefix("http://127.0.0.1:").expect("loopback URL");
    let (port, key) = rest.split_once("/?key=").unwrap();
    let port: u16 = port.parse().unwrap();
    assert_eq!(key.len(), 64, "256-bit key");

    // No key, a wrong key, a foreign Host, a non-GET method: all refused.
    assert!(http(port, "GET", "/", &[]).0.contains(" 401 "));
    assert!(
        http(port, "GET", "/api?m=project.list", &[])
            .0
            .contains(" 401 ")
    );
    assert!(http(port, "GET", "/?key=00", &[]).0.contains(" 401 "));
    let rebinding = http(
        port,
        "GET",
        &format!("/?key={key}"),
        &[("Host", "evil.example")],
    );
    assert!(rebinding.0.contains(" 421 "), "{}", rebinding.0);
    assert!(
        http(port, "POST", &format!("/?key={key}"), &[])
            .0
            .contains(" 405 ")
    );

    // The key becomes a strict cookie and leaves the address bar.
    let (status, headers, _) = http(port, "GET", &format!("/?key={key}"), &[]);
    assert!(status.contains(" 303 "), "{status}");
    assert!(headers.contains("Location: /"));
    let set_cookie = headers
        .lines()
        .find(|l| l.starts_with("Set-Cookie:"))
        .unwrap();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
    // The cookie carries a separate session secret, never the link key.
    let session = set_cookie
        .split_once("wm_ui=")
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(session.len(), 64);
    assert_ne!(session, key, "the cookie must not reuse the link key");
    // The link is single-use: a second exchange and the key as a cookie fail.
    assert!(
        http(port, "GET", &format!("/?key={key}"), &[])
            .0
            .contains(" 401 ")
    );
    let key_cookie = format!("wm_ui={key}");
    assert!(
        http(port, "GET", "/", &[("Cookie", key_cookie.as_str())])
            .0
            .contains(" 401 ")
    );
    let cookie = format!("wm_ui={session}");
    let auth = [("Cookie", cookie.as_str())];

    let (status, headers, body) = http(port, "GET", "/", &auth);
    assert!(status.contains(" 200 "));
    assert!(headers.contains("Content-Security-Policy: default-src 'none'"));
    assert!(headers.contains("X-Frame-Options: DENY"));
    assert!(body.contains("WrongMemory"));
    assert!(http(port, "GET", "/app.js", &auth).0.contains(" 200 "));

    // Read methods work; anything else is refused before reaching the daemon.
    let api = |q: &str| -> Value { serde_json::from_str(&http(port, "GET", q, &auth).2).unwrap() };
    let list = api("/api?m=project.list");
    assert_eq!(list["ok"], true);
    assert_eq!(list["result"]["projects"][0]["projectId"], project.as_str());
    let page = api(&format!("/api?m=memory.listPage&p={project}"));
    assert_eq!(
        page["result"]["memories"][0]["text"],
        "The dashboard lists this memory"
    );
    for m in [
        "memory.remember",
        "memory.delete",
        "grant.create",
        "daemon.shutdown",
        "semantic.configure",
    ] {
        let r = api(&format!(
            "/api?m={m}&p={project}&a=%7B%22text%22%3A%22x%22%7D"
        ));
        assert_eq!(r["error"]["code"], "FORBIDDEN", "{m}");
    }
    // Doctor is forced to report-only: a requested fix is ignored.
    let doc = api(&format!(
        "/api?m=memory.doctor&p={project}&a=%7B%22fixes%22%3A%5B%22fts-rebuild%22%5D%7D"
    ));
    assert_eq!(doc["ok"], true);
    assert!(doc["result"].get("fixesApplied").is_none());
    let after = api(&format!("/api?m=memory.listPage&p={project}"));
    assert_eq!(after["result"]["total"], 1, "nothing was written");

    // No credential ever reaches the page.
    let grants = http(port, "GET", "/api?m=grant.list", &auth).2;
    assert!(grants.contains(&grant_id));
    if secret.trim().len() > 8 {
        assert!(!grants.contains(secret.trim()));
    }
    assert!(!grants.to_ascii_lowercase().contains("secret"));
}
