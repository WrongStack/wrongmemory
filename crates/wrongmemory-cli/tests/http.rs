//! WM-308: `wrongmemory http serve`, optional loopback MCP over Streamable
//! HTTP. Drives the real command over raw HTTP/1.1 and checks:
//! - legacy (2025-06-18) and current (2026-07-28) requests, both stateless;
//! - no `Mcp-Session-Id` is ever issued;
//! - authentication on every request (missing, malformed, wrong token,
//!   another grant's token), all answered the same way;
//! - foreign `Host` (DNS rebinding) and any browser `Origin` refused;
//! - a grant cannot reach a project outside its allowlist by changing the
//!   URL;
//! - the daemon still enforces the grant profile (no direct write for
//!   read-propose);
//! - oversized bodies refused; revocation takes effect on the next request;
//! - no credential in any response.

#![cfg(windows)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

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

struct StopDaemon<'a>(&'a Path);

impl Drop for StopDaemon<'_> {
    fn drop(&mut self) {
        let _ = wm(self.0).args(["daemon", "stop"]).output();
    }
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {} {}", self.status, self.body))
    }
}

/// One raw HTTP/1.1 POST (`Connection: close`).
fn post(port: u16, target: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut req = format!("POST {target} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
    {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str(
        "Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n",
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let (status, headers) = head.split_once("\r\n").unwrap_or((head, ""));
    let status = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        headers: headers.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

fn meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "0"},
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

/// A current-protocol (2026-07-28) request: per-request metadata, no
/// initialize, no session.
fn modern(port: u16, project: &str, auth: &str, method: &str, mut params: Value) -> Reply {
    params["_meta"] = meta();
    let mut headers = vec![
        ("Authorization", auth),
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Mcp-Method", method),
    ];
    let name = params["name"].as_str().map(str::to_string);
    if let Some(n) = name.as_deref() {
        headers.push(("Mcp-Name", n));
    }
    post(
        port,
        &format!("/mcp/{project}"),
        &headers,
        &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
    )
}

fn tool_text(r: &Reply) -> Value {
    let v = r.json();
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no tool result: {v}"));
    serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
}

#[test]
fn loopback_http_mcp_is_authenticated_scoped_and_stateless() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _stop = StopDaemon(&home);
    let mut projects = Vec::new();
    for (name, text) in [
        (
            "a",
            "Project A ships release builds with cargo build --locked",
        ),
        (
            "b",
            "Project B secret plan: rotate the staging keys on Friday",
        ),
    ] {
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let id = run_json(
            &home,
            &["project", "add", dir.to_str().unwrap(), "--managed"],
        )["project"]["projectId"]
            .as_str()
            .unwrap()
            .to_string();
        run_json(&home, &["remember", &id, text]);
        projects.push(id);
    }
    let (a, b) = (projects[0].clone(), projects[1].clone());
    let grant = |label: &str, project: &str| {
        run_json(
            &home,
            &["grants", "create", "--label", label, "--project", project],
        )["grant"]["grantId"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (ga, gb) = (grant("agent-a", &a), grant("agent-b", &b));
    let header = |g: &str| {
        run_json(&home, &["http", "header", "--grant", g])["Authorization"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (auth_a, auth_b) = (header(&ga), header(&gb));
    assert!(auth_a.starts_with(&format!("Bearer {ga}.")));
    let secret_a = std::fs::read_to_string(home.join("grants").join(format!("{ga}.secret")))
        .unwrap()
        .trim()
        .to_string();
    assert!(
        !auth_a.contains(&secret_a),
        "the grant secret itself travels"
    );

    let mut child = wm(&home)
        .args(["http", "serve"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _server = Server(child);
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let port: u16 = line
        .trim()
        .strip_prefix("http://127.0.0.1:")
        .and_then(|r| r.split('/').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("unexpected first line {line:?}"));
    let path_a = format!("/mcp/{a}");
    let mut bodies = Vec::new();

    // Legacy revision: initialize then a request, with no session issued.
    let init = post(
        port,
        &path_a,
        &[("Authorization", &auth_a)],
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"}}})
        .to_string(),
    );
    assert_eq!(init.status, 200, "{}", init.body);
    assert_eq!(init.json()["result"]["protocolVersion"], "2025-06-18");
    assert!(!init.headers.contains("mcp-session-id"), "{}", init.headers);
    let list = post(
        port,
        &path_a,
        &[
            ("Authorization", &auth_a),
            ("MCP-Protocol-Version", "2025-06-18"),
        ],
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string(),
    );
    let names: Vec<String> = list.json()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"memory_candidates".to_string()),
        "{names:?}"
    );
    assert!(
        !names.contains(&"remember".to_string()),
        "read-propose lists remember"
    );
    bodies.push(init.body);
    bodies.push(list.body);

    // Current revision: the required per-request headers are enforced.
    let no_method = post(
        port,
        &path_a,
        &[
            ("Authorization", &auth_a),
            ("MCP-Protocol-Version", "2026-07-28"),
        ],
        &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {"_meta": meta()}})
            .to_string(),
    );
    assert_eq!(no_method.status, 400, "{}", no_method.body);
    let hits = modern(
        port,
        &a,
        &auth_a,
        "tools/call",
        json!({"name": "memory_search", "arguments": {"query": "release builds"}}),
    );
    assert_eq!(hits.status, 200, "{}", hits.body);
    assert!(!hits.headers.contains("mcp-session-id"));
    let found = tool_text(&hits);
    assert_eq!(
        found["hits"][0]["memory"]["text"],
        "Project A ships release builds with cargo build --locked"
    );
    bodies.push(hits.body);

    // The daemon enforces the profile even for a tool the list hides.
    let write = modern(
        port,
        &a,
        &auth_a,
        "tools/call",
        json!({"name": "remember", "arguments": {"text": "direct write"}}),
    );
    let refused = write.json();
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(refused.to_string().contains("FORBIDDEN"), "{refused}");

    // Authentication: every failure looks the same.
    let call = json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list"}).to_string();
    let wrong_token = format!("Bearer {ga}.{}", "0".repeat(64));
    let other_grant_token = auth_b.replacen(&gb, &ga, 1);
    let unknown_grant = format!("Bearer nosuchgrant.{}", "0".repeat(64));
    for auth in [
        None,
        Some("Bearer"),
        Some("Basic abc"),
        Some(wrong_token.as_str()),
        Some(other_grant_token.as_str()),
        Some(unknown_grant.as_str()),
        Some(secret_a.as_str()),
    ] {
        let headers: Vec<(&str, &str)> = auth.map(|v| ("Authorization", v)).into_iter().collect();
        let r = post(port, &path_a, &headers, &call);
        assert_eq!(r.status, 401, "{auth:?}: {}", r.body);
        assert_eq!(r.body, "unauthorized\n");
    }

    // A grant cannot reach another project by changing the URL.
    let cross = post(
        port,
        &format!("/mcp/{b}"),
        &[
            ("Authorization", &auth_a),
            ("MCP-Protocol-Version", "2025-06-18"),
        ],
        &call,
    );
    assert_eq!(cross.status, 403, "{}", cross.body);
    assert!(!cross.body.contains("staging keys"));
    let missing = post(
        port,
        "/mcp/p_doesnotexist",
        &[
            ("Authorization", &auth_a),
            ("MCP-Protocol-Version", "2025-06-18"),
        ],
        &call,
    );
    assert_eq!(
        missing.status, cross.status,
        "existence leaks: {}",
        missing.body
    );
    assert_eq!(missing.body, cross.body);
    // ... while its own grant reads project B.
    let own_b = modern(
        port,
        &b,
        &auth_b,
        "tools/call",
        json!({"name": "memory_search", "arguments": {"query": "staging keys"}}),
    );
    assert_eq!(tool_text(&own_b)["hits"].as_array().unwrap().len(), 1);

    // DNS rebinding and browser access.
    for (k, v) in [
        ("Host", format!("evil.example:{port}")),
        ("Host", "127.0.0.1:1".to_string()),
    ] {
        let r = post(port, &path_a, &[("Authorization", &auth_a), (k, &v)], &call);
        assert_eq!(r.status, 403, "{k}: {v}");
    }
    for origin in [
        "http://evil.example",
        &format!("http://127.0.0.1:{port}"),
        "null",
    ] {
        let r = post(
            port,
            &path_a,
            &[("Authorization", &auth_a), ("Origin", origin)],
            &call,
        );
        assert_eq!(r.status, 403, "Origin {origin}");
    }
    // Only /mcp/<project-id>.
    for target in ["/", "/mcp", "/mcp/", "/mcp/../grants", "/other"] {
        let r = post(port, target, &[("Authorization", &auth_a)], &call);
        assert_eq!(r.status, 404, "{target}");
    }
    // Oversized body, refused before it is read.
    let big = post(
        port,
        &path_a,
        &[("Authorization", &auth_a), ("Content-Length", "2000000")],
        "",
    );
    assert_eq!(big.status, 413);

    for body in &bodies {
        assert!(!body.contains(&secret_a), "a response carries the secret");
        assert!(!body.contains(&auth_a), "a response carries the token");
    }

    // Revocation applies to the very next request.
    run_json(&home, &["grants", "revoke", &ga]);
    let after = modern(
        port,
        &a,
        &auth_a,
        "tools/call",
        json!({"name": "memory_search", "arguments": {"query": "release builds"}}),
    );
    assert_eq!(after.status, 401, "{}", after.body);
}
