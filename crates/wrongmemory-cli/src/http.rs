//! `wrongmemory http serve`: optional loopback MCP over Streamable HTTP
//! (WM-308).
//!
//! One listener serves every project a caller's grant allows, at
//! `http://127.0.0.1:<port>/mcp/<project-id>`. The official `rmcp` server
//! speaks both the 2026-07-28 binding (per-request metadata, no protocol
//! sessions) and older revisions. It runs stateless for every revision: no
//! `Mcp-Session-Id` is issued, so no transport session can be shared,
//! replayed or taken over between credentials, and each request is
//! authorized by its own credential alone.
//!
//! In front of rmcp, every request passes these checks, in order:
//! - `Host` must name this listener (`127.0.0.1:<port>` or
//!   `localhost:<port>`): DNS rebinding.
//! - Any `Origin` header is refused: no browser page may call it.
//! - The path must be `/mcp/<project-id>`.
//! - `Authorization: Bearer <grant-id>.<token>`, where the token is the
//!   grant's HTTP token ([`wrongmemory_ipc::endpoint::http_token`]), checked
//!   in constant time against the grant secret in the private data root on
//!   every request. A revoked grant fails at once.
//! - The grant must include the project (the daemon checks again on every
//!   call). Unknown grant, wrong token and foreign project all answer the
//!   same 401/403 without detail.
//! - Request bodies are bounded, in-flight requests are capped, and each
//!   grant has a request budget per minute.
//!
//! Behind the checks, tool calls go through the same [`Bridge`] as the stdio
//! bridge: the daemon authenticates the grant over IPC and enforces every
//! action. MCP `clientInfo` and transport details never become memory
//! sessions or authority.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorData, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{RoleServer, ServerHandler};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Semaphore};
use tower_service::Service as _;
use wrongmemory_ipc::DataRoot;
use wrongmemory_ipc::endpoint::{ct_eq, http_token};

use crate::mcp::{Bridge, INSTRUCTIONS, tools_for};

/// Largest accepted request body.
const MAX_BODY: usize = 1024 * 1024;
/// Requests served at once across all callers.
const MAX_IN_FLIGHT: usize = 64;
/// Requests per grant per minute.
const GRANT_BUDGET_PER_MINUTE: u32 = 600;

type Body = BoxBody<Bytes, Infallible>;

/// The authenticated binding of one request, handed to the MCP handler
/// through the request extensions.
#[derive(Clone)]
struct Authed(Arc<Bridge>);

struct Budget {
    window: Instant,
    used: u32,
}

struct State {
    root: DataRoot,
    hosts: [String; 2],
    label: String,
    /// Bridges by (grant, project, token): a re-issued secret gets a new one.
    bridges: Mutex<HashMap<(String, String, String), Arc<Bridge>>>,
    budgets: std::sync::Mutex<HashMap<String, Budget>>,
    in_flight: Semaphore,
    mcp: StreamableHttpService<Endpoint, NeverSessionManager>,
}

/// Stateless MCP handler: the bridge comes from the request being served.
#[derive(Clone, Default)]
struct Endpoint;

fn bridge_of(context: &RequestContext<RoleServer>) -> Result<Arc<Bridge>, ErrorData> {
    context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|p| p.extensions.get::<Authed>())
        .map(|a| a.0.clone())
        .ok_or_else(|| ErrorData::internal_error("unauthenticated request", None))
}

impl ServerHandler for Endpoint {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "wrongmemory",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let bridge = bridge_of(&context)?;
        Ok(ListToolsResult::with_all_items(tools_for(bridge.profile())))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let bridge = bridge_of(&context)?;
        let args = request.arguments.unwrap_or_default();
        Ok(bridge.run_tool(&request.name, args).await.into())
    }
}

fn plain(status: StatusCode, msg: &'static str) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from_static(msg.as_bytes())).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    if status == StatusCode::UNAUTHORIZED {
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer realm=\"wrongmemory\""),
        );
    }
    r
}

/// `Bearer <grant-id>.<token>` -> (grant id, token).
fn parse_header(headers: &http::HeaderMap) -> Option<(String, String)> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let cred = v.strip_prefix("Bearer ")?.trim();
    let (grant, token) = cred.rsplit_once('.')?;
    let grant_ok = !grant.is_empty()
        && grant.len() <= 128
        && grant
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let token_ok = token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit());
    (grant_ok && token_ok).then(|| (grant.to_string(), token.to_ascii_lowercase()))
}

impl State {
    fn over_budget(&self, grant: &str) -> bool {
        let mut budgets = self.budgets.lock().expect("budget lock");
        let now = Instant::now();
        let b = budgets.entry(grant.to_string()).or_insert(Budget {
            window: now,
            used: 0,
        });
        if now.duration_since(b.window) >= Duration::from_secs(60) {
            b.window = now;
            b.used = 0;
        }
        b.used += 1;
        b.used > GRANT_BUDGET_PER_MINUTE
    }

    async fn bridge(
        &self,
        grant: &str,
        project: &str,
        token: &str,
    ) -> Result<Arc<Bridge>, Response<Body>> {
        let key = (grant.to_string(), project.to_string(), token.to_string());
        let mut bridges = self.bridges.lock().await;
        if let Some(b) = bridges.get(&key) {
            return Ok(b.clone());
        }
        // Drop bridges of an older secret of the same grant.
        bridges.retain(|(g, _, t), _| g != grant || t == token);
        match Bridge::connect(
            self.root.clone(),
            project.to_string(),
            grant.to_string(),
            self.label.clone(),
        )
        .await
        {
            Ok((b, _)) => {
                let b = Arc::new(b);
                bridges.insert(key, b.clone());
                Ok(b)
            }
            Err(e) if e.to_string().contains("does not include project") => {
                Err(plain(StatusCode::FORBIDDEN, "forbidden\n"))
            }
            Err(e) => {
                eprintln!("wrongmemory http: grant {grant}: {e:#}");
                Err(plain(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "memory service unavailable\n",
                ))
            }
        }
    }

    async fn handle(self: Arc<Self>, mut req: Request<Incoming>) -> Response<Body> {
        let host_ok = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| self.hosts.iter().any(|a| a.eq_ignore_ascii_case(h)));
        if !host_ok {
            return plain(StatusCode::FORBIDDEN, "host not allowed\n");
        }
        if req.headers().contains_key(header::ORIGIN) {
            return plain(StatusCode::FORBIDDEN, "browser origins are not allowed\n");
        }
        let Some(project) = req
            .uri()
            .path()
            .strip_prefix("/mcp/")
            .filter(|p| {
                !p.is_empty()
                    && p.len() <= 128
                    && p.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            })
            .map(str::to_string)
        else {
            return plain(StatusCode::NOT_FOUND, "not found\n");
        };
        let Some((grant, token)) = parse_header(req.headers()) else {
            return plain(StatusCode::UNAUTHORIZED, "unauthorized\n");
        };
        // The secret is read on every request: revocation removes it.
        let authentic = self
            .root
            .read_grant_secret(&grant)
            .ok()
            .is_some_and(|secret| ct_eq(http_token(&secret).as_bytes(), token.as_bytes()));
        if !authentic {
            return plain(StatusCode::UNAUTHORIZED, "unauthorized\n");
        }
        if self.over_budget(&grant) {
            return plain(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded\n");
        }
        let Ok(_permit) = self.in_flight.try_acquire() else {
            return plain(StatusCode::SERVICE_UNAVAILABLE, "busy\n");
        };
        if req
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .is_some_and(|n| n > MAX_BODY)
        {
            return plain(StatusCode::PAYLOAD_TOO_LARGE, "request too large\n");
        }
        let bridge = match self.bridge(&grant, &project, &token).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        req.extensions_mut().insert(Authed(bridge));
        let (parts, body) = req.into_parts();
        let req = Request::from_parts(parts, http_body_util::Limited::new(body, MAX_BODY));
        let mut mcp = self.mcp.clone();
        match mcp.call(req).await {
            Ok(r) => r,
            Err(never) => match never {},
        }
    }
}

/// The value a client sends as `Authorization` for a grant.
pub fn authorization(root: &DataRoot, grant_id: &str) -> anyhow::Result<String> {
    let secret = root
        .read_grant_secret(grant_id)
        .map_err(|e| anyhow::anyhow!("cannot read credential for grant {grant_id}: {e}"))?;
    Ok(format!("Bearer {grant_id}.{}", http_token(&secret)))
}

pub async fn serve(root: DataRoot, port: u16, label: String) -> anyhow::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let addr: SocketAddr = listener.local_addr()?;
    let port = addr.port();
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_allowed_hosts([format!("127.0.0.1:{port}"), format!("localhost:{port}")])
        .enforce_origin_validation()
        .with_max_request_body_bytes(MAX_BODY);
    let mcp = StreamableHttpService::new(
        || Ok(Endpoint),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let state = Arc::new(State {
        root,
        hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        label,
        bridges: Mutex::new(HashMap::new()),
        budgets: std::sync::Mutex::new(HashMap::new()),
        in_flight: Semaphore::new(MAX_IN_FLIGHT),
        mcp,
    });
    // The URL is the only line on stdout, so a launcher can read it.
    println!("http://127.0.0.1:{port}/mcp/<project-id>");
    eprintln!(
        "wrongmemory http: serving MCP on 127.0.0.1:{port} (loopback only); \
         send `Authorization: Bearer <grant-id>.<token>` (see `wrongmemory http header`). Ctrl+C stops."
    );
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        let (stream, _) = tokio::select! {
            r = listener.accept() => r?,
            _ = &mut shutdown => break,
        };
        let state = state.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(state.handle(req).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn bearer_shape_is_strict() {
        let ok = |v: &str| {
            let req = http::Request::builder()
                .header(http::header::AUTHORIZATION, v)
                .body(())
                .unwrap();
            let (parts, _) = req.into_parts();
            super::parse_header(&parts.headers)
        };
        let t = "a".repeat(64);
        assert_eq!(
            ok(&format!("Bearer g_1.{t}")),
            Some(("g_1".into(), t.clone()))
        );
        assert_eq!(ok(&format!("Bearer ../x.{t}")), None);
        assert_eq!(ok(&format!("Basic g_1.{t}")), None);
        assert_eq!(ok("Bearer g_1.abc"), None);
        assert_eq!(ok(&format!("Bearer .{t}")), None);
    }
}
