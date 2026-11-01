//! Connection handling, authentication, authorization, and dispatch.
//!
//! Every ingress (CLI, MCP bridge, future WrongStack adapter) goes through
//! the same principal -> project -> action checks here; MCP is a transport
//! adapter, not a second implementation of memory rules (architecture §1).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::watch;
use wrongmemory_contracts::{
    Action, AuthRequest, AuthResponse, CandidateListParams, CandidateListResult, Credential,
    EXTENSION_PROFILE_ID, EndpointInfo, ErrorBody, ErrorCode, ForPathParams, GetParams,
    GrantCreateParams, GrantCreateResult, Health, Hello, Limits, MAX_AUTH_FRAME_BYTES,
    MAX_FRAME_BYTES, MAX_PAGE_SIZE, PROTOCOL_MAJOR, PROTOCOL_MINOR, PolicyProfile, PrincipalInfo,
    ProposeParams, RegisterProjectParams, RememberParams, Request, Response, SEMANTIC_PROFILE_ID,
    STORAGE_PROFILE_ID, SearchParams, SessionAcquireParams, SessionInfo, method,
};
use wrongmemory_ipc::endpoint::{generate_token, token_eq};
use wrongmemory_ipc::frame::{FrameError, read_frame, write_frame};
use wrongmemory_ipc::{DataRoot, Discovery};
use wrongmemory_registry::{Registry, now_iso, now_ms};
use wrongmemory_storage::{MutationCtx, Viewer};

use crate::projects::{self, Stores};
use crate::{info, warn};

const AUTH_DEADLINE: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 64;

pub struct Config {
    pub root: DataRoot,
    pub drain: Duration,
}

#[derive(Debug)]
pub enum StartError {
    AlreadyRunning(String),
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for StartError {
    fn from(e: E) -> Self {
        StartError::Other(e.into())
    }
}

pub struct Shared {
    pub root: DataRoot,
    pub registry: Arc<Mutex<Registry>>,
    pub stores: Stores,
    pub instance_id: String,
    host_token: String,
    pub started_at_ms: i64,
    pub draining: AtomicBool,
    pub shutdown: watch::Sender<bool>,
    connections: AtomicUsize,
    /// In-flight blocking mutations; drained before stores close.
    inflight: AtomicUsize,
}

/// Authenticated identity of one connection. Built by the daemon only.
#[derive(Debug, Clone)]
pub struct Principal {
    pub id: String,
    pub label: String,
    pub grant: Option<GrantScope>,
}

#[derive(Debug, Clone)]
pub struct GrantScope {
    pub grant_id: String,
    pub profile: PolicyProfile,
    pub projects: Vec<String>,
}

impl Principal {
    fn info(&self) -> PrincipalInfo {
        PrincipalInfo {
            principal_id: self.id.clone(),
            kind: if self.grant.is_some() {
                "grant"
            } else {
                "host"
            }
            .into(),
            profile: self.grant.as_ref().map(|g| g.profile),
            projects: self.grant.as_ref().map(|g| g.projects.clone()),
        }
    }
}

struct Conn {
    principal: Principal,
    /// Daemon-issued or trusted-host-bound session (WM-INV-03).
    session: Option<String>,
}

fn err(code: ErrorCode, msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(code, msg)
}

impl Shared {
    async fn blocking<T, F>(self: &Arc<Self>, f: F) -> Result<T, ErrorBody>
    where
        T: Send + 'static,
        F: FnOnce(&Shared) -> Result<T, ErrorBody> + Send + 'static,
    {
        let me = self.clone();
        tokio::task::spawn_blocking(move || f(&me))
            .await
            .map_err(|e| err(ErrorCode::Internal, format!("worker failed: {e}")))?
    }

    fn reg<T>(
        &self,
        f: impl FnOnce(&mut Registry) -> Result<T, wrongmemory_registry::RegistryError>,
    ) -> Result<T, ErrorBody> {
        let mut guard = self
            .registry
            .lock()
            .map_err(|_| err(ErrorCode::Internal, "registry lock poisoned"))?;
        f(&mut guard).map_err(ErrorBody::from)
    }

    fn hello(&self, principal: &Principal) -> Hello {
        let mut capabilities = vec![
            "projects".to_string(),
            "sessions".into(),
            "memory.search".into(),
            "memory.get".into(),
            "memory.forPath".into(),
        ];
        let allows = |a: Action| principal.grant.as_ref().is_none_or(|g| g.profile.allows(a));
        if allows(Action::Propose) {
            capabilities.push("candidates".into());
        }
        if allows(Action::Write) {
            capabilities.push("memory.remember".into());
        }
        if principal.grant.is_none() {
            capabilities.extend(["grants".into(), "admin".into()]);
        }
        Hello {
            daemon_instance_id: self.instance_id.clone(),
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            storage_profile_id: STORAGE_PROFILE_ID.into(),
            extension_profile_id: EXTENSION_PROFILE_ID.into(),
            semantic_profile_id: SEMANTIC_PROFILE_ID.into(),
            capabilities,
            limits: Limits {
                max_frame_bytes: MAX_FRAME_BYTES,
                max_page_size: MAX_PAGE_SIZE,
                max_inflight: 1,
            },
            principal: principal.info(),
        }
    }
}

pub async fn run(cfg: Config) -> Result<(), StartError> {
    cfg.root
        .ensure_private_dirs()
        .with_context(|| format!("creating data root {}", cfg.root.path().display()))?;
    crate::log::init(&cfg.root.logs_dir());
    let endpoint = cfg.root.endpoint_name()?;

    // 1. Singleton: the first pipe instance is the OS-backed guard. A losing
    //    starter returns without touching the registry or discovery.
    #[cfg(windows)]
    let (listener, first) = match wrongmemory_ipc::win::PipeListener::bind(&endpoint) {
        Ok(x) => x,
        Err(e) if e.raw_os_error() == Some(5) => {
            use wrongmemory_ipc::win::{ExistingOwner, PipeListener};
            return Err(match PipeListener::existing_owner(&endpoint).await {
                ExistingOwner::SameUser { .. } => StartError::AlreadyRunning(endpoint),
                ExistingOwner::Foreign { detail } => StartError::Other(anyhow::anyhow!(
                    "endpoint {endpoint} is held by a process that is not this user's WrongMemory daemon ({detail}); refusing to start"
                )),
            });
        }
        Err(e) => return Err(anyhow::Error::new(e).context("binding endpoint").into()),
    };
    #[cfg(not(windows))]
    return Err(anyhow::anyhow!(
        "the WrongMemory daemon currently supports Windows only (Unix sockets are M8)"
    )
    .into());

    // 2. Registry.
    let registry = Registry::open(&cfg.root.registry_db())
        .map_err(|e| anyhow::anyhow!("opening registry: {e}"))?;

    // 3. Credential + discovery.
    let instance_id = format!("d_{}", uuid::Uuid::new_v4().simple());
    let started_at_ms = now_ms();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let shared = Arc::new(Shared {
        root: cfg.root.clone(),
        registry: Arc::new(Mutex::new(registry)),
        stores: Stores::default(),
        instance_id: instance_id.clone(),
        host_token: generate_token()?,
        started_at_ms,
        draining: AtomicBool::new(false),
        shutdown: shutdown_tx,
        connections: AtomicUsize::new(0),
        inflight: AtomicUsize::new(0),
    });
    let info = EndpointInfo {
        endpoint: endpoint.clone(),
        daemon_instance_id: instance_id.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        started_at_ms,
    };
    Discovery::publish(&cfg.root, &info, &shared.host_token)?;
    info!("ready", {
        "instanceId": instance_id,
        "endpoint": endpoint,
        "dataRoot": cfg.root.path().display().to_string(),
        "sqlite": wrongmemory_storage::sqlite_version(),
    });

    // 4. Accept loop.
    #[cfg(windows)]
    {
        let mut server = first;
        loop {
            tokio::select! {
                connected = server.connect() => {
                    if let Err(e) = connected {
                        warn!("accept_failed", {"error": e.to_string()});
                        server = listener.next()?;
                        continue;
                    }
                    let conn = server;
                    server = listener.next()?;
                    tokio::spawn(handle_connection(shared.clone(), conn));
                }
                _ = shutdown_rx.changed() => break,
                _ = tokio::signal::ctrl_c() => {
                    info!("signal_shutdown");
                    break;
                }
            }
        }
    }

    // 5. Drain: refuse new work, wait for in-flight mutations, close stores
    //    (releasing owner guards), retract only our own discovery metadata.
    shared.draining.store(true, Ordering::SeqCst);
    let _ = shared.shutdown.send(true);
    let deadline = tokio::time::Instant::now() + cfg.drain;
    while shared.inflight.load(Ordering::SeqCst) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for handle in shared.stores.drain_all() {
        let id = handle.info.project_id.clone();
        let epoch = handle.info.owner_epoch;
        let writable = handle.store.lock().map(|s| s.writable()).unwrap_or(false);
        drop(handle);
        if writable {
            let _ = shared.reg(|r| r.record_release(&id, epoch, &instance_id));
        }
    }
    let retracted = Discovery::retract(&cfg.root, &instance_id).unwrap_or(false);
    info!("stopped", {"instanceId": instance_id, "discoveryRetracted": retracted});
    Ok(())
}

struct ConnectionGuard(Arc<Shared>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn reply_auth<W: AsyncWrite + Unpin>(w: &mut W, r: &AuthResponse) {
    let _ = write_frame(w, r, MAX_FRAME_BYTES).await;
}

async fn handle_connection<S>(shared: Arc<Shared>, conn: S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let active = shared.connections.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = ConnectionGuard(shared.clone());
    let mut conn = BufReader::new(conn);
    if active > MAX_CONNECTIONS {
        reply_auth(
            conn.get_mut(),
            &AuthResponse::Error {
                error: err(ErrorCode::QueueSaturated, "too many connections"),
            },
        )
        .await;
        return;
    }

    let auth: AuthRequest = match tokio::time::timeout(
        AUTH_DEADLINE,
        read_frame::<_, AuthRequest>(&mut conn, MAX_AUTH_FRAME_BYTES),
    )
    .await
    {
        Ok(Ok(Some(a))) => a,
        Ok(Ok(None)) | Err(_) => return,
        Ok(Err(e)) => {
            reply_auth(
                conn.get_mut(),
                &AuthResponse::Error {
                    error: err(
                        ErrorCode::Unauthenticated,
                        format!("invalid authentication frame: {e}"),
                    ),
                },
            )
            .await;
            return;
        }
    };
    if auth.protocol_major != PROTOCOL_MAJOR {
        reply_auth(
            conn.get_mut(),
            &AuthResponse::Error {
                error: err(
                    ErrorCode::ProtocolUnsupported,
                    format!(
                        "protocol {} unsupported; daemon speaks {PROTOCOL_MAJOR}",
                        auth.protocol_major
                    ),
                ),
            },
        )
        .await;
        return;
    }
    let principal = match authenticate(&shared, &auth).await {
        Ok(p) => p,
        Err(e) => {
            warn!("auth_rejected", {"client": auth.client_name});
            reply_auth(conn.get_mut(), &AuthResponse::Error { error: e }).await;
            return;
        }
    };
    // Grant principals always run inside a fresh daemon-issued session, so
    // session-scoped rows of other sessions (and legacy unowned ones) stay
    // hidden; they cannot name a session (WM-INV-03).
    let mut state = Conn {
        session: principal.grant.as_ref().map(|_| new_session_id()),
        principal,
    };
    let hello = shared.hello(&state.principal);
    if write_frame(
        conn.get_mut(),
        &AuthResponse::Ok {
            hello: Box::new(hello),
        },
        MAX_FRAME_BYTES,
    )
    .await
    .is_err()
    {
        return;
    }
    info!("connected", {"principal": state.principal.id, "client": state.principal.label});

    let mut shutdown_rx = shared.shutdown.subscribe();
    loop {
        let next = tokio::select! {
            r = read_frame::<_, Value>(&mut conn, MAX_FRAME_BYTES) => r,
            _ = shutdown_rx.changed() => return,
        };
        let raw = match next {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(FrameError::TooLarge { max }) => {
                // The stream cannot be resynchronized after an oversized line.
                let _ = write_frame(
                    conn.get_mut(),
                    &Response::err(
                        "",
                        err(
                            ErrorCode::InvalidRequest,
                            format!("frame exceeds {max} bytes"),
                        ),
                    ),
                    MAX_FRAME_BYTES,
                )
                .await;
                return;
            }
            Err(FrameError::Invalid(e)) => {
                let resp = Response::err(
                    "",
                    err(ErrorCode::InvalidRequest, format!("malformed frame: {e}")),
                );
                if write_frame(conn.get_mut(), &resp, MAX_FRAME_BYTES)
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            Err(_) => return,
        };
        let request: Request = match serde_json::from_value(raw.clone()) {
            Ok(r) => r,
            Err(e) => {
                let id = raw.get("requestId").and_then(Value::as_str).unwrap_or("");
                let resp = Response::err(
                    id,
                    err(ErrorCode::InvalidRequest, format!("malformed request: {e}")),
                );
                if write_frame(conn.get_mut(), &resp, MAX_FRAME_BYTES)
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };
        let is_shutdown = request.method == method::DAEMON_SHUTDOWN;
        let request_id = request.request_id.clone();
        let response = match dispatch(&shared, &mut state, request).await {
            Ok(v) => Response::ok(request_id, v),
            Err(e) => Response::err(request_id, e),
        };
        let accepted = response.error.is_none();
        if write_frame(conn.get_mut(), &response, MAX_FRAME_BYTES)
            .await
            .is_err()
        {
            break;
        }
        if is_shutdown && accepted {
            info!("shutdown_requested", {"principal": state.principal.id});
            shared.draining.store(true, Ordering::SeqCst);
            let _ = shared.shutdown.send(true);
            return;
        }
    }
    info!("disconnected", {"principal": state.principal.id});
}

fn new_session_id() -> String {
    format!("ses_{}", uuid::Uuid::new_v4().simple())
}

async fn authenticate(shared: &Arc<Shared>, auth: &AuthRequest) -> Result<Principal, ErrorBody> {
    let label = auth.client_name.chars().take(64).collect::<String>();
    match &auth.credential {
        Credential::Host { token } => {
            if token_eq(token, &shared.host_token) {
                Ok(Principal {
                    id: "host".into(),
                    label,
                    grant: None,
                })
            } else {
                Err(err(ErrorCode::Unauthenticated, "invalid credential"))
            }
        }
        Credential::Grant { grant_id, secret } => {
            let (gid, secret) = (grant_id.clone(), secret.clone());
            let grant = shared
                .blocking(move |s| s.reg(|r| r.authenticate_grant(&gid, &secret)))
                .await?
                .ok_or_else(|| err(ErrorCode::Unauthenticated, "invalid credential"))?;
            Ok(Principal {
                id: format!("grant:{}", grant.grant_id),
                label,
                grant: Some(GrantScope {
                    grant_id: grant.grant_id,
                    profile: grant.profile,
                    projects: grant.projects,
                }),
            })
        }
    }
}

fn required_action(m: &str) -> Option<Action> {
    Some(match m {
        method::DAEMON_HEALTH
        | method::PROJECT_LIST
        | method::PROJECT_GET
        | method::SESSION_ACQUIRE
        | method::SESSION_RELEASE
        | method::MEMORY_SEARCH
        | method::MEMORY_GET
        | method::MEMORY_FOR_PATH => Action::Read,
        method::CANDIDATE_LIST | method::CANDIDATE_PROPOSE => Action::Propose,
        method::MEMORY_REMEMBER => Action::Write,
        method::DAEMON_SHUTDOWN
        | method::PROJECT_REGISTER
        | method::PROJECT_DETACH
        | method::GRANT_CREATE
        | method::GRANT_LIST
        | method::GRANT_REVOKE => Action::Admin,
        _ => return None,
    })
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, ErrorBody> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params)
        .map_err(|e| err(ErrorCode::InvalidRequest, format!("invalid params: {e}")))
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, ErrorBody> {
    serde_json::to_value(v).map_err(|e| err(ErrorCode::Internal, e.to_string()))
}

/// Authorize, then route. Order: method known -> action allowed for the
/// principal -> grant still active -> project in the grant's allowlist.
async fn dispatch(
    shared: &Arc<Shared>,
    state: &mut Conn,
    req: Request,
) -> Result<Value, ErrorBody> {
    if req.protocol != PROTOCOL_MAJOR {
        return Err(err(ErrorCode::ProtocolUnsupported, "protocol mismatch"));
    }
    let action = required_action(&req.method).ok_or_else(|| {
        err(
            ErrorCode::MethodNotFound,
            format!("unknown method {}", req.method),
        )
    })?;
    if let Some(g) = &state.principal.grant {
        if !g.profile.allows(action) {
            return Err(err(
                ErrorCode::Forbidden,
                format!(
                    "grant profile {} does not allow {}",
                    g.profile.as_str(),
                    req.method
                ),
            ));
        }
        // Revocation takes effect on the next request (WM-INV-16).
        let gid = g.grant_id.clone();
        if !shared
            .blocking(move |s| s.reg(|r| r.grant_active(&gid)))
            .await?
        {
            return Err(err(ErrorCode::Unauthenticated, "grant revoked or expired"));
        }
    }
    if shared.draining.load(Ordering::SeqCst) && action != Action::Read {
        return Err(err(ErrorCode::ShuttingDown, "daemon is draining"));
    }

    let project_scoped = matches!(
        req.method.as_str(),
        method::PROJECT_GET
            | method::MEMORY_SEARCH
            | method::MEMORY_GET
            | method::MEMORY_FOR_PATH
            | method::MEMORY_REMEMBER
            | method::CANDIDATE_LIST
            | method::CANDIDATE_PROPOSE
    );
    let project_id = if project_scoped {
        let id = req
            .project_id
            .clone()
            .ok_or_else(|| err(ErrorCode::InvalidRequest, "projectId is required"))?;
        if let Some(g) = &state.principal.grant
            && !g.projects.contains(&id)
        {
            // Indistinguishable from an unknown project.
            return Err(err(ErrorCode::ProjectNotFound, "project not found"));
        }
        Some(id)
    } else {
        None
    };

    match req.method.as_str() {
        method::DAEMON_HEALTH => {
            let projects = shared
                .blocking(|s| s.reg(|r| r.list_projects()))
                .await?
                .len();
            to_value(Health {
                daemon_instance_id: shared.instance_id.clone(),
                daemon_version: env!("CARGO_PKG_VERSION").into(),
                pid: std::process::id(),
                uptime_ms: now_ms() - shared.started_at_ms,
                data_root: shared.root.path().display().to_string(),
                sqlite_version: wrongmemory_storage::sqlite_version().into(),
                projects,
                open_stores: shared.stores.count(),
                connections: shared.connections.load(Ordering::SeqCst),
                draining: shared.draining.load(Ordering::SeqCst),
            })
        }
        method::DAEMON_SHUTDOWN => Ok(json!({"accepted": true})),
        method::PROJECT_REGISTER => {
            let p: RegisterProjectParams = parse(req.params)?;
            let result = shared
                .blocking(move |s| projects::register(&s.root, &s.registry, &p))
                .await?;
            info!("project_registered", {
                "projectId": result.project.project_id,
                "admission": result.project.admission,
                "created": result.created,
            });
            to_value(result)
        }
        method::PROJECT_LIST => {
            let all = shared.blocking(|s| s.reg(|r| r.list_projects())).await?;
            let visible: Vec<_> = match &state.principal.grant {
                None => all,
                Some(g) => all
                    .into_iter()
                    .filter(|p| g.projects.contains(&p.project_id))
                    .collect(),
            };
            to_value(json!({ "projects": visible }))
        }
        method::PROJECT_GET => {
            let id = project_id.expect("scoped");
            let p = shared
                .blocking(move |s| s.reg(|r| r.get_project(&id)))
                .await?
                .ok_or_else(|| err(ErrorCode::ProjectNotFound, "project not found"))?;
            to_value(p)
        }
        method::PROJECT_DETACH => {
            let id: String = parse::<Value>(req.params)?
                .get("projectId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(req.project_id)
                .ok_or_else(|| err(ErrorCode::InvalidRequest, "projectId is required"))?;
            let instance = shared.instance_id.clone();
            let detached = shared
                .blocking(move |s| {
                    if let Some(h) = s.stores.close(&id) {
                        let epoch = h.info.owner_epoch;
                        let writable = h.store.lock().map(|st| st.writable()).unwrap_or(false);
                        drop(h);
                        if writable {
                            s.reg(|r| r.record_release(&id, epoch, &instance))?;
                        }
                    }
                    s.reg(|r| r.detach_project(&id))
                })
                .await?;
            to_value(json!({ "detached": detached }))
        }
        method::GRANT_CREATE => {
            let p: GrantCreateParams = parse(req.params)?;
            if p.label.trim().is_empty() {
                return Err(err(ErrorCode::InvalidRequest, "label must not be empty"));
            }
            let result = shared
                .blocking(move |s| {
                    for id in &p.projects {
                        if s.reg(|r| r.get_project(id))?.is_none() {
                            return Err(err(
                                ErrorCode::ProjectNotFound,
                                format!("unknown project {id}"),
                            ));
                        }
                    }
                    let secret =
                        generate_token().map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                    let grant = s.reg(|r| {
                        r.create_grant(&p.label, p.profile, &p.projects, p.expires_at_ms, &secret)
                    })?;
                    s.root
                        .write_grant_secret(&grant.grant_id, &secret)
                        .map_err(|e| {
                            err(ErrorCode::Internal, format!("storing grant secret: {e}"))
                        })?;
                    Ok(GrantCreateResult { grant, secret })
                })
                .await?;
            info!("grant_created", {"grantId": result.grant.grant_id, "profile": result.grant.profile});
            // The secret is persisted owner-private; it is not echoed back.
            to_value(
                json!({ "grant": result.grant, "secretFile": shared.root.grant_secret_file(&result.grant.grant_id).map(|p| p.display().to_string()).unwrap_or_default() }),
            )
        }
        method::GRANT_LIST => {
            let grants = shared.blocking(|s| s.reg(|r| r.list_grants())).await?;
            to_value(json!({ "grants": grants }))
        }
        method::GRANT_REVOKE => {
            let id = parse::<Value>(req.params)?
                .get("grantId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| err(ErrorCode::InvalidRequest, "grantId is required"))?;
            let revoked = shared
                .blocking(move |s| {
                    let revoked = s.reg(|r| r.revoke_grant(&id))?;
                    let _ = s.root.remove_grant_secret(&id);
                    Ok(revoked)
                })
                .await?;
            to_value(json!({ "revoked": revoked }))
        }
        method::SESSION_ACQUIRE => {
            let p: SessionAcquireParams = parse(req.params)?;
            let info = match (&state.principal.grant, p.host_session_id) {
                (None, Some(host)) if !host.trim().is_empty() => SessionInfo {
                    session_id: host,
                    daemon_issued: false,
                },
                (Some(_), Some(_)) => {
                    return Err(err(
                        ErrorCode::Forbidden,
                        "grant principals cannot bind a named session",
                    ));
                }
                _ => SessionInfo {
                    session_id: new_session_id(),
                    daemon_issued: true,
                },
            };
            state.session = Some(info.session_id.clone());
            to_value(info)
        }
        method::SESSION_RELEASE => {
            // Grants keep an anonymous daemon session; hosts return to none.
            state.session = state.principal.grant.as_ref().map(|_| new_session_id());
            to_value(json!({ "released": true }))
        }
        method::MEMORY_SEARCH => {
            let p: SearchParams = parse(req.params)?;
            let viewer = Viewer {
                session_id: state.session.clone(),
            };
            let pid = project_id.expect("scoped");
            let r = shared
                .blocking(move |s| {
                    let h = s.stores.handle(&s.registry, &s.instance_id, &pid)?;
                    let store = h
                        .store
                        .lock()
                        .map_err(|_| err(ErrorCode::Internal, "store lock poisoned"))?;
                    store.search(
                        &p.query,
                        p.limit.unwrap_or(20),
                        p.kinds.as_deref(),
                        p.statuses.as_deref(),
                        &viewer,
                    )
                })
                .await?;
            to_value(r)
        }
        method::MEMORY_GET => {
            let p: GetParams = parse(req.params)?;
            let viewer = Viewer {
                session_id: state.session.clone(),
            };
            let pid = project_id.expect("scoped");
            let r = shared
                .blocking(move |s| {
                    let h = s.stores.handle(&s.registry, &s.instance_id, &pid)?;
                    let store = h
                        .store
                        .lock()
                        .map_err(|_| err(ErrorCode::Internal, "store lock poisoned"))?;
                    store.get(&p.id, &viewer)
                })
                .await?
                .ok_or_else(|| err(ErrorCode::NotFound, "memory not found"))?;
            to_value(r)
        }
        method::MEMORY_FOR_PATH => {
            let p: ForPathParams = parse(req.params)?;
            let viewer = Viewer {
                session_id: state.session.clone(),
            };
            let pid = project_id.expect("scoped");
            let r = shared
                .blocking(move |s| {
                    let h = s.stores.handle(&s.registry, &s.instance_id, &pid)?;
                    let store = h
                        .store
                        .lock()
                        .map_err(|_| err(ErrorCode::Internal, "store lock poisoned"))?;
                    store.for_path(&p.path, p.limit.unwrap_or(20), &viewer)
                })
                .await?;
            to_value(r)
        }
        method::CANDIDATE_LIST => {
            let p: CandidateListParams = parse(req.params)?;
            let viewer = Viewer {
                session_id: state.session.clone(),
            };
            let pid = project_id.expect("scoped");
            let r = shared
                .blocking(move |s| {
                    let h = s.stores.handle(&s.registry, &s.instance_id, &pid)?;
                    let store = h
                        .store
                        .lock()
                        .map_err(|_| err(ErrorCode::Internal, "store lock poisoned"))?;
                    let (candidates, truncated, corrupt_skipped) = store.list_candidates(
                        p.status.as_deref().unwrap_or("pending"),
                        p.limit.unwrap_or(50),
                        &viewer,
                    )?;
                    Ok(CandidateListResult {
                        candidates,
                        truncated,
                        corrupt_skipped,
                    })
                })
                .await?;
            to_value(r)
        }
        method::MEMORY_REMEMBER | method::CANDIDATE_PROPOSE => {
            let ctx = MutationCtx {
                principal_id: state.principal.id.clone(),
                client_label: state.principal.label.clone(),
                session_id: state.session.clone(),
                idempotency_key: req.idempotency_key.clone(),
                now_iso: now_iso(),
            };
            let pid = project_id.expect("scoped");
            let is_remember = req.method == method::MEMORY_REMEMBER;
            let params = req.params;
            shared.inflight.fetch_add(1, Ordering::SeqCst);
            let result = shared
                .blocking(move |s| {
                    let h = s.stores.handle(&s.registry, &s.instance_id, &pid)?;
                    let mut store = h
                        .store
                        .lock()
                        .map_err(|_| err(ErrorCode::Internal, "store lock poisoned"))?;
                    if is_remember {
                        let p: RememberParams = parse(params)?;
                        store.remember(&p, &ctx)
                    } else {
                        let p: ProposeParams = parse(params)?;
                        store.propose(&p, &ctx)
                    }
                })
                .await;
            shared.inflight.fetch_sub(1, Ordering::SeqCst);
            let outcome = result?;
            info!("mutation_committed", {
                "outcome": outcome.outcome, "id": outcome.id, "replayed": outcome.replayed,
                "principal": state.principal.id,
            });
            to_value(outcome)
        }
        other => Err(err(
            ErrorCode::MethodNotFound,
            format!("unknown method {other}"),
        )),
    }
}
