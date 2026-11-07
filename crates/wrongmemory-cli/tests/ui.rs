//! WM-800 (partial): `wrongmemory ui` serves a management dashboard on
//! loopback. This drives the real command over raw HTTP and checks its
//! access rules: no access without the run's key, the one-time key traded
//! for a separate `HttpOnly`/`SameSite=Strict` session cookie, foreign `Host` headers refused (DNS
//! rebinding), reads over GET from a read allowlist, changes only over
//! `POST /api` with the run's CSRF token and the page's own `Origin`, a
//! short change allowlist, and no secret in any response.

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
    http_body(port, method, target, headers, "")
}

fn http_body(
    port: u16,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
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
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let (status, headers) = head.split_once("\r\n").unwrap_or((head, ""));
    (status.to_string(), headers.to_string(), body.to_string())
}

/// Starts `wrongmemory ui` and trades its key for the session cookie.
/// Returns (ui process, port, cookie header value).
fn open_dashboard(home: &Path) -> (Ui, u16, String) {
    let mut child = wm(home)
        .args(["ui", "--no-open"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let ui = Ui(child);
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let url = line.split_whitespace().last().unwrap().to_string();
    let rest = url.strip_prefix("http://127.0.0.1:").expect("loopback URL");
    let (port, key) = rest.split_once("/?key=").unwrap();
    let port: u16 = port.parse().unwrap();
    let (_, headers, _) = http(port, "GET", &format!("/?key={key}"), &[]);
    let session = headers
        .split_once("wm_ui=")
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .to_string();
    (ui, port, format!("wm_ui={session}"))
}

/// A reader that takes the link and closes the pipe (`wrongmemory ui | head -1`,
/// or a test) must not take the dashboard down: its later writes to stdout
/// fail, and a failing write is not a reason to stop serving.
#[test]
fn dashboard_keeps_serving_after_its_stdout_is_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _stop = StopDaemon(&home);
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = wm(&home)
        .args(["ui", "--no-open", "--port", &port.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close both pipes before the dashboard writes anything.
    drop(child.stdout.take());
    drop(child.stderr.take());
    let mut ui = Ui(child);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            ui.0.try_wait().unwrap().is_none(),
            "the dashboard exited when its stdout was closed"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "dashboard never listened"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Give a failing write after the listener came up time to bring it down.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        ui.0.try_wait().unwrap().is_none(),
        "the dashboard exited when its stdout was closed"
    );
    let status = http(port, "GET", "/", &[]).0;
    assert!(status.contains(" 401 "), "{status}");
}

#[test]
fn dashboard_changes_need_the_token_and_the_pages_origin() {
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
    run_json(&home, &["remember", &project, "Edit me from the dashboard"]);
    let (_ui, port, cookie) = open_dashboard(&home);
    let origin = format!("http://127.0.0.1:{port}");
    let get = |q: &str| -> Value {
        serde_json::from_str(&http(port, "GET", q, &[("Cookie", cookie.as_str())]).2).unwrap()
    };

    // The token is served only with the session cookie.
    assert!(http(port, "GET", "/session", &[]).0.contains(" 401 "));
    let csrf = get("/session")["csrf"].as_str().unwrap().to_string();
    assert_eq!(csrf.len(), 64);

    let post =
        |headers: &[(&str, &str)], body: &str| http_body(port, "POST", "/api", headers, body);
    let memory =
        get(&format!("/api?m=memory.listPage&p={project}"))["result"]["memories"][0].clone();
    let id = memory["id"].as_str().unwrap().to_string();
    let edit = format!(
        r#"{{"m":"memory.update","p":"{project}","a":{{"id":"{id}","expectedRevision":1,"text":"Edited from the dashboard"}}}}"#
    );
    let json = ("Content-Type", "application/json");
    // Without the token, with a wrong one, from another origin, without an
    // origin, without the cookie, or as a form: refused, nothing written.
    for headers in [
        vec![
            ("Cookie", cookie.as_str()),
            ("Origin", origin.as_str()),
            json,
        ],
        vec![
            ("Cookie", cookie.as_str()),
            ("Origin", origin.as_str()),
            json,
            ("X-WrongMemory-CSRF", "00"),
        ],
        vec![
            ("Cookie", cookie.as_str()),
            ("Origin", "http://evil.example"),
            json,
            ("X-WrongMemory-CSRF", csrf.as_str()),
        ],
        vec![
            ("Cookie", cookie.as_str()),
            json,
            ("X-WrongMemory-CSRF", csrf.as_str()),
        ],
        vec![
            ("Cookie", cookie.as_str()),
            ("Origin", origin.as_str()),
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("X-WrongMemory-CSRF", csrf.as_str()),
        ],
    ] {
        let (status, _, body) = post(&headers, &edit);
        assert!(status.contains(" 403 "), "{headers:?}: {status} {body}");
    }
    let (status, _, _) = post(
        &[
            ("Origin", origin.as_str()),
            json,
            ("X-WrongMemory-CSRF", csrf.as_str()),
        ],
        &edit,
    );
    assert!(status.contains(" 401 "), "{status}");
    assert_eq!(
        get(&format!(
            "/api?m=memory.get&p={project}&a=%7B%22id%22%3A%22{id}%22%7D"
        ))["result"]["text"],
        "Edit me from the dashboard"
    );

    let ok: [(&str, &str); 4] = [
        ("Cookie", cookie.as_str()),
        ("Origin", origin.as_str()),
        json,
        ("X-WrongMemory-CSRF", csrf.as_str()),
    ];
    let call = |body: String| -> Value { serde_json::from_str(&post(&ok, &body).2).unwrap() };

    // Methods outside the change list are refused before the daemon.
    for m in [
        "grant.create",
        "daemon.shutdown",
        "project.export",
        "memory.remember",
    ] {
        let r = call(format!(
            r#"{{"m":"{m}","p":"{project}","a":{{"text":"x"}}}}"#
        ));
        assert_eq!(r["error"]["code"], "FORBIDDEN", "{m}");
    }
    // An edit must name the revision it was made from.
    let r = call(format!(
        r#"{{"m":"memory.update","p":"{project}","a":{{"id":"{id}","text":"no revision"}}}}"#
    ));
    assert_eq!(r["error"]["code"], "INVALID_REQUEST");

    // With all of it: the edit lands; a stale revision is refused.
    let r = call(edit.clone());
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(
        get(&format!(
            "/api?m=memory.get&p={project}&a=%7B%22id%22%3A%22{id}%22%7D"
        ))["result"]["text"],
        "Edited from the dashboard"
    );
    let r = call(edit);
    assert_eq!(r["error"]["code"], "REVISION_CONFLICT", "{r}");

    // Delete and recover.
    let r = call(format!(
        r#"{{"m":"memory.delete","p":"{project}","a":{{"id":"{id}","force":true,"reason":"test"}}}}"#
    ));
    assert_eq!(r["ok"], true, "{r}");
    let page = get(&format!("/api?m=memory.listPage&p={project}"));
    assert_eq!(page["result"]["total"], 0);
    let r = call(format!(
        r#"{{"m":"memory.recover","p":"{project}","a":{{"id":"{id}"}}}}"#
    ));
    assert_eq!(r["ok"], true, "{r}");

    // A maintenance run is queued with the project's own options.
    let r = call(format!(
        r#"{{"m":"maintenance.run","p":"{project}","a":{{"options":{{"purgeDeletedAfterDays":0}}}}}}"#
    ));
    assert_eq!(r["ok"], true, "{r}");

    // Removing the project detaches it; the store file stays.
    let store = run_json(&home, &["project", "list"])["projects"][0]["storePath"]
        .as_str()
        .unwrap()
        .to_string();
    let r = call(format!(r#"{{"m":"project.detach","p":"{project}"}}"#));
    assert_eq!(r["result"]["detached"], true, "{r}");
    assert_eq!(
        get("/api?m=project.list")["result"]["projects"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert!(Path::new(&store).exists(), "detach keeps the store file");
}

#[test]
fn dashboard_is_loopback_keyed_and_reads_over_get() {
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

    // Read methods work over GET; changes are not reachable that way.
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
