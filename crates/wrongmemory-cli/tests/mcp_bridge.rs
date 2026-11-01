//! MCP conformance smoke for the stdio bridge: real `wrongmemory mcp`
//! process, real daemon (auto-started by the bridge), newline-delimited
//! JSON-RPC over pipes. Covers discovery vs. enforcement, stdout cleanliness,
//! idempotent proposals, and "bridge EOF leaves the daemon running".

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

fn wm(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_wrongmemory"));
    c.arg("--data-root").arg(home);
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

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(home: &Path, project: &str, grant: &str) -> Self {
        let mut child = wm(home)
            .args([
                "mcp",
                "--project",
                project,
                "--grant",
                grant,
                "--client",
                "test",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next: 0,
        }
    }

    fn send(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let msg = json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "bridge closed stdout"
            );
            // stdout carries protocol frames only.
            let v: Value = serde_json::from_str(&line).expect("non-JSON line on stdout");
            if v["id"] == json!(self.next) {
                return v;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "method": method})).unwrap();
        stdin.flush().unwrap();
    }

    fn tool(&mut self, name: &str, args: Value) -> Value {
        self.send("tools/call", json!({"name": name, "arguments": args}))["result"].clone()
    }
}

#[test]
fn bridge_serves_scoped_tools_and_exits_on_eof() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();

    // `project add` auto-starts the daemon.
    let reg = run_json(
        &home,
        &["project", "add", proj.to_str().unwrap(), "--managed"],
    );
    let project = reg["project"]["projectId"].as_str().unwrap().to_string();
    run_json(
        &home,
        &[
            "remember",
            &project,
            "Release builds use cargo build --locked",
            "--kind",
            "convention",
            "--file",
            "Cargo.toml",
        ],
    );
    let g = run_json(
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
    let grant = g["grant"]["grantId"].as_str().unwrap().to_string();

    let mut mcp = Mcp::start(&home, &project, &grant);
    let init = mcp.send(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "wrongmemory");
    mcp.notify("notifications/initialized");

    let tools = mcp.send("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "memory_search",
            "memory_for_path",
            "memory_for_file",
            "memory_get",
            "memory_graph",
            "memory_gather_batch",
            "memory_related",
            "memory_feedback",
            "memory_candidates"
        ]
    );

    let r = mcp.tool("memory_search", json!({"query": "cargo locked"}));
    assert_eq!(r["isError"], false);
    assert_eq!(r["structuredContent"]["hits"].as_array().unwrap().len(), 1);
    let r = mcp.tool("memory_for_path", json!({"path": "Cargo.toml"}));
    assert_eq!(r["structuredContent"]["hits"].as_array().unwrap().len(), 1);
    let r = mcp.tool(
        "memory_for_file",
        json!({"path": "Cargo.toml", "showDeleted": false}),
    );
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["filePath"], "Cargo.toml");
    assert_eq!(r["structuredContent"]["totalCount"], 1);

    let args = json!({"action": "propose", "text": "Integration tests spawn the real daemon binary", "paths": ["crates/wrongmemoryd/tests"]});
    let p1 = mcp.tool("memory_candidates", args.clone());
    let p2 = mcp.tool("memory_candidates", args);
    assert_eq!(p1["structuredContent"]["outcome"], "candidate-pending");
    assert_eq!(p1["structuredContent"]["id"], p2["structuredContent"]["id"]);
    assert_eq!(p2["structuredContent"]["replayed"], true);

    // Not listed AND enforced by the daemon.
    let r = mcp.tool("remember", json!({"text": "forged"}));
    assert_eq!(r["isError"], true);
    assert!(
        r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("FORBIDDEN")
    );

    // EOF: the bridge exits cleanly; the daemon keeps serving.
    drop(mcp.stdin.take());
    let status = mcp.child.wait().unwrap();
    assert!(status.success(), "bridge exit {status}");
    let health = run_json(&home, &["daemon", "status"]);
    assert!(
        health["daemonInstanceId"]
            .as_str()
            .unwrap()
            .starts_with("d_")
    );
    let pending = run_json(&home, &["candidates", "list", &project]);
    assert_eq!(pending["candidates"].as_array().unwrap().len(), 1);

    let out = wm(&home).args(["daemon", "stop"]).output().unwrap();
    assert!(out.status.success());
}
