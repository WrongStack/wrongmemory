//! `wrongmemory ui`: a local management dashboard in the browser.
//!
//! The CLI process serves one page on `127.0.0.1` and forwards two
//! allowlists of IPC methods to the daemon over its host connection:
//! * reads, over `GET /api` (`memory.doctor` forced to its report-only form);
//! * a short list of changes (project detach, memory edit/delete/recover,
//!   candidate accept/reject, a maintenance run, named doctor repairs) over
//!   `POST /api`. Each change carries the run's CSRF token in a header, comes
//!   from the page's own origin, and is reduced to the fields the page needs
//!   (an edit must name the revision it read). Everything else, such as grant
//!   creation, export or daemon shutdown, is refused before the daemon.
//!
//! Exposure is limited to this user on this machine:
//! * loopback only, on an ephemeral port unless `--port` is given;
//! * a fresh 256-bit one-time key per run. Its first use trades it for a
//!   separate 256-bit session secret in an `HttpOnly`, `SameSite=Strict`
//!   cookie, and the key stops working, so a key seen in a terminal, a
//!   browser history or a process listing is worthless once the page opened;
//! * the browser is opened through a launcher file in the owner-only runtime
//!   directory, never with the key in a process argument (`/proc/*/cmdline`
//!   and process listings are visible to other local users);
//! * the `Host` header must name the loopback listener (no DNS rebinding);
//! * GET, plus `POST /api` with the run's CSRF token (served only to a
//!   request carrying the session cookie) and a same-origin `Origin`;
//!   a 16 KiB header cap and a 256 KiB body cap, a strict
//!   Content-Security-Policy, `frame-ancestors 'none'` and `no-store` caching.
//!
//! The host token and grant secrets never reach the page: the daemon
//! connection lives in this process and grant listings carry no secret.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use wrongmemory_contracts::method;
use wrongmemory_ipc::{CallError, Client, DataRoot};

const INDEX_HTML: &str = include_str!("ui_assets/index.html");
const APP_JS: &str = include_str!("ui_assets/app.js");
const APP_CSS: &str = include_str!("ui_assets/app.css");

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 256 * 1024;
const COOKIE: &str = "wm_ui";
const CSRF_HEADER: &str = "x-wrongmemory-csrf";

/// Read methods the page may call over GET, and whether each is project-scoped.
const READS: &[(&str, bool)] = &[
    (method::DAEMON_HEALTH, false),
    (method::PROJECT_LIST, false),
    (method::GRANT_LIST, false),
    (method::PROJECT_GET, true),
    (method::MEMORY_STATS, true),
    (method::MEMORY_LIST_PAGE, true),
    (method::MEMORY_SEARCH, true),
    (method::MEMORY_GET, true),
    (method::CANDIDATE_LIST, true),
    (method::MAINTENANCE_STATUS, true),
    (method::JOBS_LIST, true),
    (method::EVENTS_SINCE, true),
    (method::SEMANTIC_STATUS, true),
    (method::MEMORY_AUDIT, true),
    (method::MEMORY_DOCTOR, true),
    (method::MEMORY_GRAPH, true),
    (method::MEMORY_RELATED, true),
    (method::JOBS_GET, true),
];

/// Changes the page may make over POST. All are project-scoped.
const WRITES: &[&str] = &[
    method::PROJECT_DETACH,
    method::MEMORY_UPDATE,
    method::MEMORY_DELETE,
    method::MEMORY_RECOVER,
    method::CANDIDATE_ACCEPT,
    method::CANDIDATE_REJECT,
    method::MAINTENANCE_RUN,
    method::MEMORY_DOCTOR,
];

/// Fields `memory.update` may carry from the page.
const UPDATE_FIELDS: &[&str] = &[
    "id",
    "expectedRevision",
    "text",
    "kind",
    "status",
    "tags",
    "anchors",
    "importance",
    "confidence",
    "validity",
    "persistence",
];

struct Ui {
    root: DataRoot,
    /// One-time key from the printed link; `None` once exchanged.
    url_key: std::sync::Mutex<Option<String>>,
    /// Cookie value issued by the exchange.
    session: String,
    /// Token every change must carry in [`CSRF_HEADER`].
    csrf: String,
    port: u16,
    client: Mutex<Option<Client>>,
}

fn random_hex() -> anyhow::Result<String> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

pub async fn run(root: DataRoot, port: u16, open: bool) -> anyhow::Result<()> {
    let client = crate::spawn::connect_host(&root, "wrongmemory-ui", true)
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach the daemon: {e}"))?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    let key = random_hex()?;
    let url = format!("http://127.0.0.1:{port}/?key={key}");
    let ui = Arc::new(Ui {
        root,
        url_key: std::sync::Mutex::new(Some(key)),
        session: random_hex()?,
        csrf: random_hex()?,
        port,
        client: Mutex::new(Some(client)),
    });
    println!("WrongMemory dashboard: {url}");
    println!("The link works once; press Ctrl+C to stop.");
    if open {
        open_browser(&ui.root, port, &url);
    }
    loop {
        let (stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let ui = ui.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(30), serve(ui, stream)).await;
        });
    }
}

/// Path of the launcher page for `port` (owner-only runtime directory).
fn launcher_path(root: &DataRoot, port: u16) -> std::path::PathBuf {
    root.runtime_dir().join(format!("ui-{port}.html"))
}

/// Open the browser on a private launcher page that redirects to `url`, so
/// the key never appears in a process argument.
fn open_browser(root: &DataRoot, port: u16, url: &str) {
    let page = launcher_path(root, port);
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"0; url={url}\">         <title>WrongMemory</title><a href=\"{url}\">Open the WrongMemory dashboard</a>"
    );
    if let Err(e) = wrongmemory_ipc::endpoint::write_private_atomic(&page, html.as_bytes()) {
        eprintln!("Could not write the launcher ({e}); open the link above yourself.");
        return;
    }
    #[cfg(windows)]
    let r = std::process::Command::new("explorer.exe")
        .arg(&page)
        .spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(&page).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(&page).spawn();
    if r.is_err() {
        eprintln!("Could not open a browser; open the link above yourself.");
    }
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 || buf.len() + n > MAX_HEAD + MAX_BODY {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_HEAD && !buf[..MAX_HEAD].windows(4).any(|w| w == b"\r\n\r\n") {
            return None;
        }
    };
    let mut body = buf.split_off(head_end + 4);
    buf.truncate(head_end);
    let text = String::from_utf8(buf).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines.take_while(|l| !l.is_empty()) {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let (path, qs) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let query = qs
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Some((percent_decode(k)?, percent_decode(v)?))
        })
        .collect();
    let length = match headers.get("content-length") {
        Some(v) => v.parse::<usize>().ok()?,
        None => 0,
    };
    if length > MAX_BODY || headers.contains_key("transfer-encoding") {
        return None;
    }
    while body.len() < length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Some(Request {
        method,
        path: path.to_string(),
        query,
        headers,
        body,
    })
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn cookie_key(req: &Request) -> Option<&str> {
    req.headers.get("cookie")?.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == COOKIE).then_some(v)
    })
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, String)],
) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; \
         connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; \
         form-action 'none'\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.shutdown().await;
}

async fn serve(ui: Arc<Ui>, mut stream: TcpStream) {
    let Some(req) = read_request(&mut stream).await else {
        return respond(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"bad request",
            &[],
        )
        .await;
    };
    let host_ok = req.headers.get("host").is_some_and(|h| {
        h == &format!("127.0.0.1:{}", ui.port) || h == &format!("localhost:{}", ui.port)
    });
    if !host_ok {
        return respond(
            &mut stream,
            "421 Misdirected Request",
            "text/plain",
            b"wrong host",
            &[],
        )
        .await;
    }
    let post = req.method == "POST";
    if !(req.method == "GET" || post && req.path == "/api" && !req.query.contains_key("key")) {
        return respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"GET, or POST /api",
            &[],
        )
        .await;
    }
    // First visit: trade the one-time key for a session cookie, retire the
    // key, and drop it from the address bar.
    if let Some(k) = req.query.get("key") {
        let accepted = req.path == "/" && {
            let mut slot = ui.url_key.lock().unwrap_or_else(|p| p.into_inner());
            let ok = slot.as_deref().is_some_and(|key| constant_eq(k, key));
            if ok {
                *slot = None;
            }
            ok
        };
        if accepted {
            let _ = std::fs::remove_file(launcher_path(&ui.root, ui.port));
            let cookie = format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/", ui.session);
            return respond(
                &mut stream,
                "303 See Other",
                "text/plain",
                b"",
                &[("Set-Cookie", cookie), ("Location", "/".into())],
            )
            .await;
        }
        return respond(
            &mut stream,
            "401 Unauthorized",
            "text/plain",
            b"invalid key",
            &[],
        )
        .await;
    }
    if !cookie_key(&req).is_some_and(|k| constant_eq(k, &ui.session)) {
        return respond(
            &mut stream,
            "401 Unauthorized",
            "text/plain; charset=utf-8",
            "Open the link printed by `wrongmemory ui`.".as_bytes(),
            &[],
        )
        .await;
    }
    if post {
        if let Err(why) = change_allowed(&ui, &req) {
            return respond(
                &mut stream,
                "403 Forbidden",
                "text/plain; charset=utf-8",
                why.as_bytes(),
                &[],
            )
            .await;
        }
        let body = change(&ui, &req.body).await;
        return respond(
            &mut stream,
            "200 OK",
            "application/json",
            body.to_string().as_bytes(),
            &[],
        )
        .await;
    }
    match req.path.as_str() {
        "/session" => {
            // Same-origin only: no CORS header is ever sent, so another site
            // cannot read the token even with the cookie attached.
            let body = json!({
                "csrf": ui.csrf,
                "version": env!("CARGO_PKG_VERSION"),
            });
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                body.to_string().as_bytes(),
                &[],
            )
            .await
        }
        "/" => {
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                INDEX_HTML.as_bytes(),
                &[],
            )
            .await
        }
        "/app.js" => {
            respond(
                &mut stream,
                "200 OK",
                "text/javascript; charset=utf-8",
                APP_JS.as_bytes(),
                &[],
            )
            .await
        }
        "/app.css" => {
            respond(
                &mut stream,
                "200 OK",
                "text/css; charset=utf-8",
                APP_CSS.as_bytes(),
                &[],
            )
            .await
        }
        "/api" => {
            let body = api(&ui, &req.query).await;
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                body.to_string().as_bytes(),
                &[],
            )
            .await
        }
        _ => {
            respond(
                &mut stream,
                "404 Not Found",
                "text/plain",
                b"not found",
                &[],
            )
            .await
        }
    }
}

async fn api(ui: &Ui, q: &HashMap<String, String>) -> Value {
    let m = q.get("m").map(String::as_str).unwrap_or_default();
    let Some(&(m, scoped)) = READS.iter().find(|(name, _)| *name == m) else {
        return json!({"ok": false, "error": {"code": "FORBIDDEN", "message": "not a dashboard method"}});
    };
    let project = q.get("p").filter(|p| !p.is_empty()).cloned();
    if scoped && project.is_none() {
        return json!({"ok": false, "error": {"code": "INVALID_REQUEST", "message": "project required"}});
    }
    let mut args: Value = match q.get("a") {
        Some(a) => match serde_json::from_str(a) {
            Ok(v @ Value::Object(_)) => v,
            _ => {
                return json!({"ok": false, "error": {"code": "INVALID_REQUEST", "message": "args must be a JSON object"}});
            }
        },
        None => json!({}),
    };
    if m == method::MEMORY_DOCTOR {
        args = json!({}); // report only: repairs go through POST
    }
    match call(ui, m, project.as_deref(), args, None).await {
        Ok(v) => json!({"ok": true, "result": v}),
        Err(e) => json!({"ok": false, "error": error_json(&e)}),
    }
}

/// Checks a `POST /api` before its body is read: same origin and the run's
/// CSRF token (the session cookie was checked by the caller).
fn change_allowed(ui: &Ui, req: &Request) -> Result<(), &'static str> {
    let origin_ok = req.headers.get("origin").is_some_and(|o| {
        o == &format!("http://127.0.0.1:{}", ui.port)
            || o == &format!("http://localhost:{}", ui.port)
    });
    if !origin_ok {
        return Err("changes must come from the dashboard page (Origin)");
    }
    let json = req.headers.get("content-type").is_some_and(|c| {
        c.split(';')
            .next()
            .is_some_and(|t| t.trim() == "application/json")
    });
    if !json {
        return Err("changes must be application/json");
    }
    if !req
        .headers
        .get(CSRF_HEADER)
        .is_some_and(|t| constant_eq(t, &ui.csrf))
    {
        return Err("missing or wrong dashboard token");
    }
    Ok(())
}

fn refuse(code: &str, message: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": message}})
}

/// Reduces a change to the fields the page may send for its method.
fn change_args(m: &str, project: &str, a: &serde_json::Map<String, Value>) -> Result<Value, Value> {
    let pick = |keys: &[&str]| -> serde_json::Map<String, Value> {
        keys.iter()
            .filter_map(|k| a.get(*k).map(|v| ((*k).to_string(), v.clone())))
            .collect()
    };
    let need_id = |o: &serde_json::Map<String, Value>| {
        if o.get("id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            Ok(())
        } else {
            Err(refuse("INVALID_REQUEST", "id is required"))
        }
    };
    let out = match m {
        method::PROJECT_DETACH => json!({ "projectId": project }),
        method::MEMORY_UPDATE => {
            let o = pick(UPDATE_FIELDS);
            need_id(&o)?;
            if !o.get("expectedRevision").is_some_and(Value::is_i64) {
                return Err(refuse(
                    "INVALID_REQUEST",
                    "an edit must name the revision it was made from (expectedRevision)",
                ));
            }
            Value::Object(o)
        }
        // SAGE asks every delete for `force`; the page sends it after its own
        // confirmation (a separate one for permanent memories), and the
        // store audits the override.
        method::MEMORY_DELETE => {
            let o = pick(&["id", "reason", "neverInject", "force"]);
            need_id(&o)?;
            Value::Object(o)
        }
        method::MEMORY_RECOVER | method::CANDIDATE_REJECT => {
            let o = pick(&["id", "reason"]);
            need_id(&o)?;
            Value::Object(o)
        }
        method::CANDIDATE_ACCEPT => {
            let o = pick(&["id"]);
            need_id(&o)?;
            Value::Object(o)
        }
        // The project's own schedule options; none from the page.
        method::MAINTENANCE_RUN => json!({}),
        method::MEMORY_DOCTOR => {
            let fixes: Vec<String> = a
                .get("fix")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            if fixes.is_empty() {
                return Err(refuse("INVALID_REQUEST", "name the repairs to apply (fix)"));
            }
            json!({ "fix": fixes })
        }
        _ => return Err(refuse("FORBIDDEN", "not a dashboard change")),
    };
    Ok(out)
}

/// `POST /api` body: `{"m": method, "p": projectId, "a": {...}}`.
async fn change(ui: &Ui, body: &[u8]) -> Value {
    let Ok(Value::Object(req)) = serde_json::from_slice::<Value>(body) else {
        return refuse("INVALID_REQUEST", "body must be a JSON object");
    };
    let m = req.get("m").and_then(Value::as_str).unwrap_or_default();
    let Some(&m) = WRITES.iter().find(|w| **w == m) else {
        return refuse("FORBIDDEN", "not a dashboard change");
    };
    let Some(project) = req
        .get("p")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    else {
        return refuse("INVALID_REQUEST", "project required");
    };
    let empty = serde_json::Map::new();
    let a = match req.get("a") {
        None => &empty,
        Some(Value::Object(o)) => o,
        Some(_) => return refuse("INVALID_REQUEST", "args must be a JSON object"),
    };
    let args = match change_args(m, project, a) {
        Ok(v) => v,
        Err(e) => return e,
    };
    // A fresh key per change: should the reply be lost, the daemon replays
    // the receipt instead of applying the change twice. Changes are never
    // retried here.
    let key = match random_hex() {
        Ok(k) => k,
        Err(e) => return refuse("INTERNAL", &e.to_string()),
    };
    match call(ui, m, Some(project), args, Some(&key)).await {
        Ok(v) => json!({"ok": true, "result": v}),
        Err(e) => json!({"ok": false, "error": error_json(&e)}),
    }
}

fn error_json(e: &CallError) -> Value {
    match e {
        CallError::Remote(b) => json!({"code": b.code.as_str(), "message": b.message}),
        other => json!({"code": "UNAVAILABLE", "message": other.to_string()}),
    }
}

/// One call over the shared host connection. A read reconnects and retries
/// once if the daemon restarted in the meantime; a change (with `key`) is
/// sent once and only reconnects for the next call.
async fn call(
    ui: &Ui,
    m: &str,
    p: Option<&str>,
    args: Value,
    key: Option<&str>,
) -> Result<Value, CallError> {
    let mut slot = ui.client.lock().await;
    let attempts = if key.is_some() { 1 } else { 2 };
    let mut last = None;
    for _ in 0..attempts {
        if slot.is_none() {
            *slot = Some(crate::spawn::connect_host(&ui.root, "wrongmemory-ui", true).await?);
        }
        let c = slot.as_mut().expect("connected");
        match c.call_raw(m, p, args.clone(), key).await {
            Err(CallError::Remote(b)) => return Err(CallError::Remote(b)),
            Err(e) => {
                *slot = None;
                last = Some(e);
            }
            ok => return ok,
        }
    }
    Err(last.expect("at least one attempt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_and_key_comparison() {
        assert_eq!(percent_decode("a%20b+c%7B%7D").as_deref(), Some("a b c{}"));
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%e2%82%ac").as_deref(), Some("€"));
        assert!(constant_eq("abc", "abc"));
        assert!(!constant_eq("abc", "abd"));
        assert!(!constant_eq("abc", "ab"));
    }

    /// Mutating methods the page must never reach, over GET or POST.
    const NEVER: &[&str] = &[
        method::DAEMON_SHUTDOWN,
        method::PROJECT_REGISTER,
        method::PROJECT_EXPORT,
        method::PROJECT_BIND_CHECKOUT,
        method::GRANT_CREATE,
        method::GRANT_REVOKE,
        method::MEMORY_REMEMBER,
        method::MEMORY_FEEDBACK,
        method::MEMORY_RECORD_USAGE,
        method::MEMORY_BACKFILL,
        method::MEMORY_HYGIENE,
        method::MEMORY_VERIFY,
        method::MEMORY_IMPORT_LEGACY,
        method::MEMORY_HQ_SYNC_APPLY,
        method::MEMORY_LEGACY_FORGET,
        method::MEMORY_LEGACY_CONSOLIDATE,
        method::MEMORY_LEGACY_CLEAR,
        method::CANDIDATE_PROPOSE,
        method::CANDIDATE_RESOLVE,
        method::MAINTENANCE_CONFIGURE,
        method::JOBS_CANCEL,
        method::SEMANTIC_CONFIGURE,
        method::SESSION_ACQUIRE,
        method::SESSION_RELEASE,
    ];

    #[test]
    fn every_method_is_classified_and_reads_never_mutate() {
        for (m, _) in READS {
            assert!(!NEVER.contains(m), "{m} must not be reachable");
            assert!(
                !WRITES.contains(m) || *m == method::MEMORY_DOCTOR,
                "{m} is both a read and a change"
            );
            assert!(method::ALL.contains(m), "{m} is not a method");
        }
        for m in WRITES {
            assert!(!NEVER.contains(m), "{m} must not be reachable");
            assert!(method::ALL.contains(m), "{m} is not a method");
        }
        // Every method is classified: read, change, never, or a read the
        // page does not use.
        let unlisted_reads = [
            method::MEMORY_FOR_PATH,
            method::MEMORY_FOR_AUDIENCE,
            method::MEMORY_FOR_FILE,
            method::MEMORY_UNIFIED_SEARCH,
            method::MEMORY_HQ_SYNC_VERSION,
            method::MEMORY_HQ_SYNC_LIST,
        ];
        for m in method::ALL {
            let known = READS.iter().any(|(a, _)| a == m)
                || WRITES.contains(m)
                || NEVER.contains(m)
                || unlisted_reads.contains(m);
            assert!(known, "classify {m}");
        }
    }

    #[test]
    fn changes_are_reduced_to_their_fields() {
        let o = |v: Value| v.as_object().cloned().unwrap();
        // Delete keeps only its own fields.
        let d = change_args(
            method::MEMORY_DELETE,
            "p",
            &o(json!({"id": "m1", "force": true, "reason": "r", "status": "active"})),
        )
        .unwrap();
        assert_eq!(d, json!({"id": "m1", "force": true, "reason": "r"}));
        // An edit must name its revision; unknown fields are dropped.
        assert!(
            change_args(
                method::MEMORY_UPDATE,
                "p",
                &o(json!({"id": "m1", "text": "t"}))
            )
            .is_err()
        );
        let u = change_args(
            method::MEMORY_UPDATE,
            "p",
            &o(json!({"id": "m1", "expectedRevision": 3, "text": "t", "scope": "project"})),
        )
        .unwrap();
        assert_eq!(u, json!({"id": "m1", "expectedRevision": 3, "text": "t"}));
        // Detach always targets the request's own project.
        let p = change_args(
            method::PROJECT_DETACH,
            "p_a",
            &o(json!({"projectId": "p_b"})),
        )
        .unwrap();
        assert_eq!(p, json!({"projectId": "p_a"}));
        // Maintenance takes the schedule's options, never the page's.
        let r = change_args(
            method::MAINTENANCE_RUN,
            "p",
            &o(json!({"options": {"purgeDeletedAfterDays": 0}})),
        )
        .unwrap();
        assert_eq!(r, json!({}));
        assert!(change_args(method::MEMORY_DOCTOR, "p", &o(json!({}))).is_err());
        assert!(change_args(method::GRANT_CREATE, "p", &o(json!({}))).is_err());
    }
}
