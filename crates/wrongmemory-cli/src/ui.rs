//! `wrongmemory ui`: a local, read-only dashboard in the browser.
//!
//! The CLI process serves one page on `127.0.0.1` and forwards an allowlist
//! of read-only IPC methods to the daemon over its host connection. Nothing
//! the page can request writes: the allowlist holds read methods only, and
//! `memory.doctor` is forced to its report-only form.
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
//! * GET only, a 16 KiB request cap, a strict Content-Security-Policy,
//!   `frame-ancestors 'none'` and `no-store` caching.
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

const MAX_REQUEST: usize = 16 * 1024;
const COOKIE: &str = "wm_ui";

/// Read-only methods the page may call, and whether each is project-scoped.
const ALLOWED: &[(&str, bool)] = &[
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
];

struct Ui {
    root: DataRoot,
    /// One-time key from the printed link; `None` once exchanged.
    url_key: std::sync::Mutex<Option<String>>,
    /// Cookie value issued by the exchange.
    session: String,
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
        port,
        client: Mutex::new(Some(client)),
    });
    println!("WrongMemory dashboard (read-only): {url}");
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
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 || buf.len() + n > MAX_REQUEST {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
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
    Some(Request {
        method,
        path: path.to_string(),
        query,
        headers,
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
    if req.method != "GET" {
        return respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"GET only",
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
    match req.path.as_str() {
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
    let Some(&(m, scoped)) = ALLOWED.iter().find(|(name, _)| *name == m) else {
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
        args = json!({}); // report only: never apply fixes from the page
    }
    match call(ui, m, project.as_deref(), args).await {
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

/// One call over the shared host connection; reconnects once if the daemon
/// restarted in the meantime.
async fn call(ui: &Ui, m: &str, p: Option<&str>, args: Value) -> Result<Value, CallError> {
    let mut slot = ui.client.lock().await;
    for attempt in 0..2 {
        if slot.is_none() {
            *slot = Some(crate::spawn::connect_host(&ui.root, "wrongmemory-ui", true).await?);
        }
        let c = slot.as_mut().expect("connected");
        match c.call_raw(m, p, args.clone(), None).await {
            Err(CallError::Remote(b)) => return Err(CallError::Remote(b)),
            Err(e) if attempt == 0 => {
                let _ = e;
                *slot = None;
            }
            other => return other,
        }
    }
    unreachable!("the second attempt always returns")
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

    #[test]
    fn allowlist_holds_no_mutating_method() {
        let mutating = [
            method::DAEMON_SHUTDOWN,
            method::PROJECT_REGISTER,
            method::PROJECT_DETACH,
            method::PROJECT_EXPORT,
            method::GRANT_CREATE,
            method::GRANT_REVOKE,
            method::MEMORY_REMEMBER,
            method::MEMORY_UPDATE,
            method::MEMORY_FEEDBACK,
            method::MEMORY_DELETE,
            method::MEMORY_RECORD_USAGE,
            method::MEMORY_RECOVER,
            method::MEMORY_BACKFILL,
            method::MEMORY_HYGIENE,
            method::MEMORY_VERIFY,
            method::MEMORY_IMPORT_LEGACY,
            method::MEMORY_HQ_SYNC_APPLY,
            method::MEMORY_LEGACY_FORGET,
            method::MEMORY_LEGACY_CONSOLIDATE,
            method::MEMORY_LEGACY_CLEAR,
            method::CANDIDATE_PROPOSE,
            method::CANDIDATE_ACCEPT,
            method::CANDIDATE_REJECT,
            method::CANDIDATE_RESOLVE,
            method::MAINTENANCE_CONFIGURE,
            method::MAINTENANCE_RUN,
            method::JOBS_CANCEL,
            method::SEMANTIC_CONFIGURE,
            method::SESSION_ACQUIRE,
            method::SESSION_RELEASE,
        ];
        for (m, _) in ALLOWED {
            assert!(
                !mutating.contains(m),
                "{m} must not be reachable from the page"
            );
            assert!(method::ALL.contains(m), "{m} is not a method");
        }
        // Every method is classified: allowed (read) or mutating.
        for m in method::ALL {
            let known = ALLOWED.iter().any(|(a, _)| a == m) || mutating.contains(m);
            let unlisted_reads = [
                method::MEMORY_FOR_PATH,
                method::MEMORY_RELATED,
                method::MEMORY_FOR_AUDIENCE,
                method::MEMORY_FOR_FILE,
                method::MEMORY_UNIFIED_SEARCH,
                method::MEMORY_GRAPH,
                method::MEMORY_HQ_SYNC_VERSION,
                method::MEMORY_HQ_SYNC_LIST,
                method::JOBS_GET,
            ];
            assert!(known || unlisted_reads.contains(m), "classify {m}");
        }
    }
}
