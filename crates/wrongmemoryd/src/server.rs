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
    STORAGE_PROFILE_ID, SearchParams, SemanticConfigureParams, SessionAcquireParams, SessionInfo,
    method,
};
use wrongmemory_contracts::{
    Admission, EventsSinceParams, EventsSinceResult, JobInfo, MaintenanceConfigureParams, StoreKind,
};
use wrongmemory_contracts::{
    BindCheckoutParams, CHECKOUT_LINKED_WORKTREE, CHECKOUT_MAIN, CheckoutInfo,
};
use wrongmemory_ipc::endpoint::{generate_token, token_eq};
use wrongmemory_ipc::frame::{FrameError, read_frame, write_frame};
use wrongmemory_ipc::{DataRoot, Discovery};
use wrongmemory_registry::jobs::JobAdmission;
use wrongmemory_registry::{Registry, now_iso, now_ms};
use wrongmemory_storage::mutate::UsageKind;
use wrongmemory_storage::{MutationCtx, Viewer};

use crate::events::{self, Events};
use crate::projects::{self, Stores, invalidates_handle};
use crate::scheduler::{self, Scheduler};
use crate::{info, warn};

const AUTH_DEADLINE: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 64;

pub struct Config {
    pub root: DataRoot,
    pub drain: Duration,
    /// Cap on simultaneously open stores (LRU eviction of idle handles).
    pub max_open_stores: usize,
    /// Maintenance scheduler tick.
    pub scheduler_tick: Duration,
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
    pub(crate) inflight: AtomicUsize,
    pub events: Events,
    pub scheduler: Scheduler,
    pub semantic: crate::semantic::SemanticCaches,
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
    /// Per-connection id, echoed as change-event `origin` (not a secret).
    connection_id: String,
    principal: Principal,
    /// Daemon-issued or trusted-host-bound session (WM-INV-03).
    session: Option<String>,
    /// Host-bound linked worktree (`project.bindCheckout`, WM-INV-14). Unset
    /// for the project's own root and for grants: evidence is the shared one.
    checkout: Option<BoundCheckout>,
}

struct BoundCheckout {
    project_id: String,
    root: std::path::PathBuf,
}

impl Conn {
    /// The linked worktree this connection is bound to for `project_id`.
    fn checkout_for(&self, project_id: &str) -> Option<&std::path::Path> {
        self.checkout
            .as_ref()
            .filter(|c| c.project_id == project_id)
            .map(|c| c.root.as_path())
    }
}

/// Absolute paths inside a bound linked worktree are rewritten relative to it,
/// so the shared store reads them as the same project path; the main
/// checkout's root would reject them as outside the project. Relative paths
/// are already checkout-independent, and other absolute paths are left as
/// they are (the store applies its own containment rules).
fn rebase_checkout_paths(m: &str, params: &mut Value, root: &std::path::Path) {
    let rebase = |v: &mut Value| {
        if let Some(s) = v.as_str()
            && std::path::Path::new(s).is_absolute()
            && let Ok(rel) = wrongmemory_storage::paths::normalize_project_path(root, s)
        {
            *v = Value::String(rel);
        }
    };
    let each = |list: Option<&mut Value>| {
        if let Some(items) = list.and_then(Value::as_array_mut) {
            items.iter_mut().for_each(rebase);
        }
    };
    match m {
        method::MEMORY_FOR_PATH | method::MEMORY_FOR_FILE => {
            if let Some(p) = params.get_mut("path") {
                rebase(p);
            }
        }
        method::MEMORY_VERIFY => each(params.get_mut("paths")),
        method::MEMORY_UNIFIED_SEARCH => each(params.pointer_mut("/query/paths")),
        method::MEMORY_REMEMBER | method::MEMORY_UPDATE | method::CANDIDATE_PROPOSE => {
            if let Some(anchors) = params.get_mut("anchors").and_then(Value::as_array_mut) {
                for a in anchors {
                    if let Some(p) = a.get_mut("path") {
                        rebase(p);
                    }
                }
            }
        }
        _ => {}
    }
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

    pub(crate) fn reg<T>(
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
            connection_id: String::new(),
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
        stores: Stores::new(cfg.max_open_stores, cfg.root.path().to_path_buf()),
        instance_id: instance_id.clone(),
        host_token: generate_token()?,
        started_at_ms,
        draining: AtomicBool::new(false),
        shutdown: shutdown_tx,
        connections: AtomicUsize::new(0),
        inflight: AtomicUsize::new(0),
        events: Events::new(events::DEFAULT_RING),
        scheduler: Scheduler::new(cfg.scheduler_tick),
        semantic: Default::default(),
    });
    {
        let me = shared.clone();
        tokio::task::spawn_blocking(move || scheduler::recover(&me))
            .await
            .map_err(|e| anyhow::anyhow!("scheduler recovery: {e}"))?
            .map_err(|e| anyhow::anyhow!("scheduler recovery: {}", e.message))?;
    }
    let scheduler_task = tokio::spawn(scheduler::run(shared.clone()));
    let info = EndpointInfo {
        endpoint: endpoint.clone(),
        daemon_instance_id: instance_id.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        started_at_ms,
    };
    crate::fault::lifecycle_pause().await;
    Discovery::publish(&cfg.root, &info, &shared.host_token)?;
    info!("ready", {
        "instanceId": instance_id,
        "endpoint": endpoint,
        "dataRoot": cfg.root.path().display().to_string(),
        "sqlite": wrongmemory_storage::sqlite_version(),
    });
    let discovery_guard = tokio::spawn(guard_discovery(
        shared.clone(),
        cfg.root.clone(),
        info.clone(),
        shutdown_rx.clone(),
    ));

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
    crate::fault::release_pause().await;
    let deadline = tokio::time::Instant::now() + cfg.drain;
    while shared.inflight.load(Ordering::SeqCst) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The scheduler records a running job's outcome before it exits; past
    // the drain deadline the job stays `running` and the next start marks
    // it interrupted.
    let _ = tokio::time::timeout(
        deadline.saturating_duration_since(tokio::time::Instant::now()),
        scheduler_task,
    )
    .await;
    for handle in shared.stores.drain_all() {
        projects::release(handle, &shared.registry, &instance_id, "shutdown");
    }
    // The guard must be gone before we retract, or it would republish.
    discovery_guard.abort();
    let _ = discovery_guard.await;
    crate::fault::lifecycle_pause().await;
    let retracted = Discovery::retract(&cfg.root, &instance_id).unwrap_or(false);
    info!("stopped", {"instanceId": instance_id, "discoveryRetracted": retracted});
    Ok(())
}

/// How often the live daemon checks that discovery still names it.
const DISCOVERY_CHECK: Duration = Duration::from_millis(500);

/// Keep this daemon's discovery metadata in place while it serves.
///
/// An exiting predecessor retracts discovery with a check-then-delete
/// (`Discovery::retract`); if this daemon publishes inside that window, the
/// predecessor deletes the new files and clients find no daemon although one
/// runs (hosted CI run 41342819145). Only the daemon that owns the endpoint
/// is live, so it simply republishes whatever is missing or foreign.
async fn guard_discovery(
    shared: Arc<Shared>,
    root: DataRoot,
    info: EndpointInfo,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(DISCOVERY_CHECK) => {}
            _ = shutdown.changed() => return,
        }
        if shared.draining.load(Ordering::SeqCst) {
            return;
        }
        let intact = Discovery::load(&root).is_ok_and(|d| {
            d.info.daemon_instance_id == info.daemon_instance_id
                && token_eq(&d.token, &shared.host_token)
        });
        if !intact {
            match Discovery::publish(&root, &info, &shared.host_token) {
                Ok(()) => warn!("discovery_restored", {"instanceId": info.daemon_instance_id}),
                Err(e) => warn!("discovery_restore_failed", {"error": e.to_string()}),
            }
        }
    }
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

    // Optional pre-auth challenge: the daemon proves it holds the host token
    // before the client reveals any credential (pipe-squatting defense for
    // clients that cannot check the server's OS identity).
    let first = match tokio::time::timeout(
        AUTH_DEADLINE,
        read_frame::<_, Value>(&mut conn, MAX_AUTH_FRAME_BYTES),
    )
    .await
    {
        Ok(Ok(Some(v))) => v,
        _ => return,
    };
    let first = if first.get("type").and_then(Value::as_str) == Some("challenge") {
        let nonce = first.get("nonce").and_then(Value::as_str).unwrap_or("");
        if nonce.len() < 32 || nonce.len() > 128 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return;
        }
        let proof = wrongmemory_ipc::endpoint::server_proof(&shared.host_token, nonce);
        if write_frame(
            conn.get_mut(),
            &json!({"type": "proof", "proof": proof}),
            MAX_FRAME_BYTES,
        )
        .await
        .is_err()
        {
            return;
        }
        match tokio::time::timeout(
            AUTH_DEADLINE,
            read_frame::<_, Value>(&mut conn, MAX_AUTH_FRAME_BYTES),
        )
        .await
        {
            Ok(Ok(Some(v))) => v,
            _ => return,
        }
    } else {
        first
    };
    let auth: AuthRequest = match serde_json::from_value::<AuthRequest>(first) {
        Ok(a) => a,
        Err(e) => {
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
        connection_id: format!("c_{}", uuid::Uuid::new_v4().simple()),
        session: principal.grant.as_ref().map(|_| new_session_id()),
        principal,
        checkout: None,
    };
    let mut hello = shared.hello(&state.principal);
    hello.connection_id = state.connection_id.clone();
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
        let method_name = request.method.clone();
        let response = match dispatch(&shared, &mut state, request).await {
            Ok(v) => {
                crate::fault::after_commit_before_reply(&method_name);
                Response::ok(request_id, v)
            }
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
        | method::MEMORY_FOR_PATH
        | method::MEMORY_LIST_PAGE
        | method::MEMORY_RELATED
        | method::MEMORY_FOR_AUDIENCE
        | method::MEMORY_FOR_FILE
        | method::MEMORY_UNIFIED_SEARCH
        | method::MEMORY_GRAPH
        | method::MEMORY_STATS
        | method::MAINTENANCE_STATUS
        | method::JOBS_LIST
        | method::JOBS_GET
        | method::EVENTS_SINCE
        | method::SEMANTIC_STATUS => Action::Read,
        method::CANDIDATE_LIST
        | method::CANDIDATE_PROPOSE
        | method::MEMORY_FEEDBACK
        | method::MEMORY_RECORD_USAGE => Action::Propose,
        method::MEMORY_REMEMBER | method::MEMORY_UPDATE => Action::Write,
        method::MEMORY_DELETE
        | method::CANDIDATE_ACCEPT
        | method::CANDIDATE_REJECT
        | method::CANDIDATE_RESOLVE
        | method::MEMORY_RECOVER
        | method::MEMORY_BACKFILL
        | method::MEMORY_HYGIENE
        | method::MEMORY_VERIFY
        | method::MEMORY_IMPORT_LEGACY
        | method::MEMORY_LEGACY_FORGET
        | method::MEMORY_LEGACY_CONSOLIDATE
        | method::MEMORY_LEGACY_CLEAR
        | method::MAINTENANCE_CONFIGURE
        | method::MAINTENANCE_RUN
        | method::JOBS_CANCEL
        | method::SEMANTIC_CONFIGURE => Action::Curate,
        method::DAEMON_SHUTDOWN
        | method::PROJECT_REGISTER
        | method::PROJECT_DETACH
        | method::PROJECT_EXPORT
        // A grant cannot claim a checkout; its evidence is the shared one.
        | method::PROJECT_BIND_CHECKOUT
        // Store-wide report / audit trail across every session: host only.
        | method::MEMORY_DOCTOR
        | method::MEMORY_AUDIT
        // HQ replication of shared memories: the trusted host's publisher only.
        | method::MEMORY_HQ_SYNC_VERSION
        | method::MEMORY_HQ_SYNC_LIST
        | method::MEMORY_HQ_SYNC_APPLY
        | method::GRANT_CREATE
        | method::GRANT_LIST
        | method::GRANT_REVOKE => Action::Admin,
        _ => return None,
    })
}

/// WM-601: hand a managed project back to in-process SAGE. Under the
/// project's writer lane, snapshot the managed store into the project's
/// legacy location (`<root>/.wrongstack/memories/sage.db`), verify the copy,
/// then retire the handle and detach the registration, so no write can land
/// in the managed store after the snapshot and two live stores never
/// diverge. The managed store files are kept. An occupied legacy location
/// is refused: export never overwrites or merges into an existing store.
fn export_project(s: &Shared, id: &str, instance: &str) -> Result<Value, ErrorBody> {
    let h = s.stores.handle(&s.registry, instance, id)?;
    if h.info.store_kind != StoreKind::Managed {
        return Err(err(
            ErrorCode::InvalidRequest,
            "only a managed store can be exported; an adopted store already lives in the project",
        ));
    }
    let target = std::path::Path::new(&h.info.root)
        .join(".wrongstack")
        .join("memories")
        .join("sage.db");
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let p = std::path::PathBuf::from(format!("{}{suffix}", target.display()));
        if p.exists() {
            return Err(err(
                ErrorCode::StoreIdentityConflict,
                format!(
                    "{} already exists; export never overwrites or merges into an existing store",
                    p.display()
                ),
            ));
        }
    }
    let lane = h.lock()?;
    let report = wrongmemory_storage::backup::backup_store(
        std::path::Path::new(&h.info.store_path),
        &target,
    )?;
    let verdict = wrongmemory_storage::inspect::inspect(&target).verdict;
    if !verdict.readable() {
        let _ = std::fs::remove_file(&target);
        return Err(err(
            ErrorCode::ProfileUnsupported,
            format!(
                "exported copy is not a readable SAGE store ({verdict:?}); nothing was detached"
            ),
        ));
    }
    h.retire();
    let detached = s.reg(|r| r.detach_project(id))?;
    drop(lane);
    s.stores.invalidate(id, &s.registry, instance, "exported");
    s.semantic.drop_project(&s.root, id);
    Ok(json!({
        "projectId": id,
        "legacyStorePath": target.display().to_string(),
        "managedStorePath": h.info.store_path,
        "verdict": format!("{verdict:?}"),
        "backup": serde_json::to_value(&report).unwrap_or(Value::Null),
        "detached": detached,
    }))
}

fn mutation_ctx(state: &Conn, idempotency_key: Option<String>) -> MutationCtx {
    MutationCtx {
        principal_id: state.principal.id.clone(),
        client_label: state.principal.label.clone(),
        session_id: state.session.clone(),
        idempotency_key,
        now_iso: now_iso(),
    }
}

fn obj(params: Value) -> Result<serde_json::Map<String, Value>, ErrorBody> {
    match params {
        Value::Null => Ok(serde_json::Map::new()),
        Value::Object(m) => Ok(m),
        _ => Err(err(ErrorCode::InvalidRequest, "params must be an object")),
    }
}

/// Run `f` against the project's store on the blocking pool, serialized by
/// the store mutex (the project's writer lane).
async fn store_op<T, F>(shared: &Arc<Shared>, pid: String, f: F) -> Result<T, ErrorBody>
where
    T: Send + 'static,
    F: FnOnce(&mut wrongmemory_storage::SageStore) -> Result<T, ErrorBody> + Send + 'static,
{
    shared
        .blocking(move |s| {
            let h = s.stores.handle(&s.registry, &s.instance_id, &pid)?;
            let mut store = h.lock()?;
            f(&mut store)
        })
        .await
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, ErrorBody> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params)
        .map_err(|e| err(ErrorCode::InvalidRequest, format!("invalid params: {e}")))
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, ErrorBody> {
    serde_json::to_value(v).map_err(|e| err(ErrorCode::Internal, e.to_string()))
}

/// Dispatch plus post-commit effects: successful mutations publish change
/// events (after the store transaction committed); store failures drop the
/// cached handle so a later request reopens it (WM-208).
async fn dispatch(
    shared: &Arc<Shared>,
    state: &mut Conn,
    req: Request,
) -> Result<Value, ErrorBody> {
    let m = req.method.clone();
    let pid = req.project_id.clone();
    let result = dispatch_inner(shared, state, req).await;
    if let Some(pid) = pid {
        match &result {
            Ok(v) if m.starts_with("memory.") || m.starts_with("candidate.") => {
                let mut drafts = events::drafts_for(&m, v);
                for d in &mut drafts {
                    d.event.origin = Some(state.connection_id.clone());
                }
                shared.events.publish(&pid, drafts);
            }
            Err(e) if invalidates_handle(e) => {
                warn!("store_unavailable", {"projectId": pid, "code": e.code.as_str()});
                let me = shared.clone();
                let _ = me
                    .blocking(move |s| {
                        s.stores
                            .invalidate(&pid, &s.registry, &s.instance_id, "store_error");
                        Ok(())
                    })
                    .await;
            }
            _ => {}
        }
    }
    result
}

/// Authorize, then route. Order: method known -> action allowed for the
/// principal -> grant still active -> project in the grant's allowlist.
async fn dispatch_inner(
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

    let project_scoped = req.method == method::PROJECT_GET
        || req.method == method::PROJECT_BIND_CHECKOUT
        || req.method.starts_with("memory.")
        || req.method.starts_with("candidate.")
        || req.method.starts_with("maintenance.")
        || req.method.starts_with("events.")
        || req.method.starts_with("semantic.")
        || (req.method == method::JOBS_LIST && req.project_id.is_some());
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
    let mut req = req;
    if let Some(root) = project_id.as_deref().and_then(|id| state.checkout_for(id)) {
        rebase_checkout_paths(&req.method, &mut req.params, root);
    }

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
                .blocking(move |s| {
                    let (result, upgraded) = projects::register(&s.root, &s.registry, &p)?;
                    if upgraded {
                        // A read-only handle opened before the upgrade would
                        // keep refusing writes; the next call reopens it.
                        s.stores.invalidate(
                            &result.project.project_id,
                            &s.registry,
                            &s.instance_id,
                            "admission_upgraded",
                        );
                    }
                    Ok(result)
                })
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
        method::PROJECT_BIND_CHECKOUT => {
            let p: BindCheckoutParams = parse(req.params)?;
            let id = project_id.expect("scoped");
            let lookup = id.clone();
            let (project_root, linked) = shared
                .blocking(move |s| {
                    let project = s
                        .reg(|r| r.get_project(&lookup))?
                        .ok_or_else(|| err(ErrorCode::ProjectNotFound, "project not found"))?;
                    let linked = projects::resolve_checkout(&project.root, &p.root)?;
                    Ok((project.root, linked))
                })
                .await?;
            let info = match &linked {
                Some(root) => CheckoutInfo {
                    root: root.display().to_string(),
                    kind: CHECKOUT_LINKED_WORKTREE.into(),
                },
                None => CheckoutInfo {
                    root: project_root,
                    kind: CHECKOUT_MAIN.into(),
                },
            };
            state.checkout = linked.map(|root| BoundCheckout {
                project_id: id,
                root,
            });
            to_value(json!({ "checkout": info }))
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
                    let adopted_store = s
                        .reg(|r| r.get_project(&id))?
                        .filter(|p| p.store_kind == StoreKind::Adopted)
                        .map(|p| std::path::PathBuf::from(p.store_path));
                    s.stores.invalidate(&id, &s.registry, &instance, "detached");
                    s.semantic.drop_project(&s.root, &id);
                    let detached = s.reg(|r| r.detach_project(&id))?;
                    // Released: a host may open the store locally again.
                    if detached
                        && let Some(store) = adopted_store
                        && let Err(e) = wrongmemory_storage::owner::remove_adoption_marker(&store)
                    {
                        warn!("adoption_marker_remove_failed", {
                            "projectId": id,
                            "error": e.to_string(),
                        });
                    }
                    Ok(detached)
                })
                .await?;
            to_value(json!({ "detached": detached }))
        }
        method::PROJECT_EXPORT => {
            let id: String = parse::<Value>(req.params)?
                .get("projectId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(req.project_id)
                .ok_or_else(|| err(ErrorCode::InvalidRequest, "projectId is required"))?;
            let instance = shared.instance_id.clone();
            let v = shared
                .blocking(move |s| export_project(s, &id, &instance))
                .await?;
            to_value(v)
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
            let mode = p.semantic.clone().unwrap_or_else(|| "lexical".into());
            if !wrongmemory_contracts::SEMANTIC_MODES.contains(&mode.as_str()) {
                return Err(err(
                    ErrorCode::InvalidRequest,
                    "semantic must be lexical, auto or required",
                ));
            }
            let r = shared
                .blocking(move |s| crate::semantic::search(s, &pid, &p, &mode, &viewer))
                .await?;
            to_value(r)
        }
        method::SEMANTIC_STATUS | method::SEMANTIC_CONFIGURE => {
            let pid = project_id.expect("scoped");
            let configure = if req.method == method::SEMANTIC_CONFIGURE {
                Some(parse::<SemanticConfigureParams>(req.params)?)
            } else {
                None
            };
            let r = shared
                .blocking(move |s| match configure {
                    Some(p) => crate::semantic::configure(s, &pid, &p.provider),
                    None => crate::semantic::status(s, &pid),
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
                    let store = h.lock()?;
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
                    let store = h.lock()?;
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
                    let store = h.lock()?;
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
            let is_grant = state.principal.grant.is_some();
            let params = req.params;
            shared.inflight.fetch_add(1, Ordering::SeqCst);
            let result = shared
                .blocking(move |s| {
                    let h = s.stores.handle(&s.registry, &s.instance_id, &pid)?;
                    let mut store = h.lock()?;
                    if is_remember {
                        let p: RememberParams = parse(params)?;
                        store.remember(&p, &ctx)
                    } else {
                        let p: ProposeParams = parse(params)?;
                        if is_grant && p.scope.as_deref() == Some("session") {
                            return Err(err(
                                ErrorCode::InvalidRequest,
                                "scope session is not available for external proposals; use project, file, symbol, or user.",
                            ));
                        }
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
        method::MEMORY_UPDATE
        | method::MEMORY_FEEDBACK
        | method::MEMORY_DELETE
        | method::MEMORY_RECORD_USAGE
        | method::CANDIDATE_ACCEPT
        | method::CANDIDATE_REJECT
        | method::CANDIDATE_RESOLVE
        | method::MEMORY_RECOVER
        | method::MEMORY_BACKFILL
        | method::MEMORY_HYGIENE
        | method::MEMORY_VERIFY
        | method::MEMORY_IMPORT_LEGACY
        | method::MEMORY_DOCTOR
        | method::MEMORY_AUDIT
        // HQ replication of shared memories: the trusted host's publisher only.
        | method::MEMORY_HQ_SYNC_VERSION
        | method::MEMORY_HQ_SYNC_LIST
        | method::MEMORY_HQ_SYNC_APPLY
        | method::MEMORY_LEGACY_FORGET
        | method::MEMORY_LEGACY_CONSOLIDATE
        | method::MEMORY_LEGACY_CLEAR => {
            let ctx = mutation_ctx(state, req.idempotency_key.clone());
            let curator = state
                .principal
                .grant
                .as_ref()
                .is_none_or(|g| g.profile.allows(Action::Curate));
            let pid = project_id.expect("scoped");
            let checkout = state.checkout_for(&pid).map(std::path::Path::to_path_buf);
            let m = req.method.clone();
            let params = obj(req.params)?;
            shared.inflight.fetch_add(1, Ordering::SeqCst);
            let result = store_op(shared, pid, move |store| {
                let id = || {
                    params
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| err(ErrorCode::InvalidRequest, "id is required"))
                };
                match m.as_str() {
                    method::MEMORY_UPDATE => {
                        let mut input = params.clone();
                        input.remove("id");
                        if input.contains_key("feedback") {
                            return Err(err(
                                ErrorCode::InvalidRequest,
                                "use memory.feedback for feedback",
                            ));
                        }
                        to_value(store.update(&id()?, &input, &ctx, curator)?)
                    }
                    method::MEMORY_FEEDBACK => {
                        let mut input = serde_json::Map::new();
                        input.insert(
                            "feedback".into(),
                            params.get("feedback").cloned().ok_or_else(|| {
                                err(ErrorCode::InvalidRequest, "feedback is required")
                            })?,
                        );
                        to_value(store.update(&id()?, &input, &ctx, false)?)
                    }
                    method::MEMORY_DELETE => {
                        let reason = params
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("Deleted via WrongMemory");
                        let force = params
                            .get("force")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let never = params
                            .get("neverInject")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        to_value(store.delete(&id()?, reason, force, never, &ctx, curator)?)
                    }
                    method::MEMORY_RECORD_USAGE => {
                        let ids: Vec<String> =
                            serde_json::from_value(params.get("ids").cloned().unwrap_or(json!([])))
                                .map_err(|_| {
                                    err(
                                        ErrorCode::InvalidRequest,
                                        "ids must be an array of strings",
                                    )
                                })?;
                        let kind = match params.get("kind").and_then(Value::as_str) {
                            Some("injection") => UsageKind::Injection,
                            Some("use") => UsageKind::Use,
                            _ => {
                                return Err(err(
                                    ErrorCode::InvalidRequest,
                                    "kind must be injection or use",
                                ));
                            }
                        };
                        let label = params
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or("external");
                        let n = store.record_usage(&ids, kind, label, &ctx)?;
                        Ok(json!({ "recorded": n }))
                    }
                    method::CANDIDATE_ACCEPT => to_value(store.accept_candidate(&id()?, &ctx)?),
                    method::MEMORY_RECOVER => {
                        let reason = params.get("reason").and_then(Value::as_str);
                        to_value(store.recover(&id()?, reason, &ctx)?)
                    }
                    method::MEMORY_BACKFILL => {
                        let filter: wrongmemory_storage::recovery::BackfillFilter =
                            serde_json::from_value(
                                params.get("filter").cloned().unwrap_or(json!({})),
                            )
                            .map_err(|e| {
                                err(ErrorCode::InvalidRequest, format!("invalid filter: {e}"))
                            })?;
                        let apply = params
                            .get("apply")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        to_value(store.backfill_recoverable(&filter, !apply, &ctx)?)
                    }
                    method::MEMORY_HYGIENE => {
                        let opts: wrongmemory_storage::hygiene::HygieneOptions =
                            serde_json::from_value(Value::Object(params.clone())).map_err(|e| {
                                err(ErrorCode::InvalidRequest, format!("invalid options: {e}"))
                            })?;
                        to_value(store.hygiene(&opts, &ctx)?)
                    }
                    method::MEMORY_DOCTOR => {
                        let fix: Vec<String> =
                            serde_json::from_value(params.get("fix").cloned().unwrap_or(json!([])))
                                .map_err(|_| {
                                    err(
                                        ErrorCode::InvalidRequest,
                                        "fix must be an array of strings",
                                    )
                                })?;
                        let opts: wrongmemory_storage::hygiene::HygieneOptions =
                            serde_json::from_value(
                                params.get("hygieneOptions").cloned().unwrap_or(json!({})),
                            )
                            .map_err(|e| {
                                err(
                                    ErrorCode::InvalidRequest,
                                    format!("invalid hygieneOptions: {e}"),
                                )
                            })?;
                        store.doctor(&fix, &opts, &ctx)
                    }
                    method::MEMORY_HQ_SYNC_VERSION => {
                        to_value(json!({"version": store.hq_sync_version()?}))
                    }
                    method::MEMORY_HQ_SYNC_LIST => {
                        let after = match params.get("after") {
                            None | Some(Value::Null) => String::new(),
                            Some(Value::String(s)) => s.clone(),
                            Some(_) => {
                                return Err(err(ErrorCode::InvalidRequest, "Invalid SAGE sync cursor"));
                            }
                        };
                        to_value(json!({"records": store.list_hq_sync(&after)?}))
                    }
                    method::MEMORY_HQ_SYNC_APPLY => {
                        let records = params
                            .get("records")
                            .and_then(Value::as_array)
                            .ok_or_else(|| err(ErrorCode::InvalidRequest, "Invalid SAGE sync records"))?;
                        let changed = store.apply_hq_sync(records, &ctx)?;
                        to_value(json!({"changed": changed}))
                    }
                    method::MEMORY_AUDIT => {
                        let limit = params.get("limit").and_then(Value::as_f64);
                        if limit.is_some_and(|l| !(1.0..=1000.0).contains(&l)) {
                            return Err(err(ErrorCode::InvalidRequest, "limit must be 1..1000"));
                        }
                        to_value(json!({"records": store.read_audit(limit)?}))
                    }
                    method::MEMORY_VERIFY => {
                        let results = match params.get("paths") {
                            Some(p) => {
                                let paths: Vec<String> = serde_json::from_value(p.clone())
                                    .map_err(|_| {
                                        err(
                                            ErrorCode::InvalidRequest,
                                            "paths must be an array of strings",
                                        )
                                    })?;
                                if paths.len() > 1000 {
                                    return Err(err(
                                        ErrorCode::InvalidRequest,
                                        "at most 1000 paths per call",
                                    ));
                                }
                                store.verify_for_paths(&paths, &ctx, checkout.as_deref())?
                            }
                            None => {
                                let id = params.get("id").and_then(Value::as_str);
                                store.verify(id, &ctx, checkout.as_deref())?
                            }
                        };
                        Ok(json!({ "results": results }))
                    }
                    method::MEMORY_IMPORT_LEGACY => {
                        let files: Vec<String> = serde_json::from_value(
                            params.get("files").cloned().unwrap_or(json!([])),
                        )
                        .map_err(|_| {
                            err(
                                ErrorCode::InvalidRequest,
                                "files must be an array of strings",
                            )
                        })?;
                        if files.is_empty() || files.len() > 100 {
                            return Err(err(
                                ErrorCode::InvalidRequest,
                                "files must name 1 to 100 project files",
                            ));
                        }
                        let (imported, skipped, n) = store.import_legacy_files(&files, &ctx)?;
                        Ok(json!({"imported": imported, "skipped": skipped, "files": n}))
                    }
                    method::MEMORY_LEGACY_FORGET => {
                        let query = params
                            .get("query")
                            .and_then(Value::as_str)
                            .ok_or_else(|| err(ErrorCode::InvalidRequest, "query is required"))?;
                        let scope = params
                            .get("scope")
                            .and_then(Value::as_str)
                            .unwrap_or("project-memory");
                        to_value(store.legacy_forget(query, scope, &ctx)?)
                    }
                    method::MEMORY_LEGACY_CONSOLIDATE => {
                        let scope = params
                            .get("scope")
                            .and_then(Value::as_str)
                            .ok_or_else(|| err(ErrorCode::InvalidRequest, "scope is required"))?;
                        to_value(store.legacy_consolidate(scope, &ctx)?)
                    }
                    method::MEMORY_LEGACY_CLEAR => {
                        let scope = params.get("scope").and_then(Value::as_str);
                        // Clearing every scope must be asked for explicitly.
                        if scope.is_none()
                            && params.get("all").and_then(Value::as_bool) != Some(true)
                        {
                            return Err(err(
                                ErrorCode::InvalidRequest,
                                "pass a scope, or all=true to clear every scope",
                            ));
                        }
                        to_value(store.legacy_clear(scope, &ctx)?)
                    }
                    method::CANDIDATE_REJECT => {
                        let reason = params
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("Rejected");
                        to_value(store.reject_candidate(&id()?, reason, &ctx)?)
                    }
                    _ => {
                        let decision = params.get("decision").and_then(Value::as_str).unwrap_or("");
                        let reason = params.get("reason").and_then(Value::as_str);
                        to_value(store.resolve_candidate(
                            &id()?,
                            decision,
                            reason,
                            &ctx,
                            curator,
                        )?)
                    }
                }
            })
            .await;
            shared.inflight.fetch_sub(1, Ordering::SeqCst);
            result
        }
        method::MEMORY_LIST_PAGE
        | method::MEMORY_RELATED
        | method::MEMORY_FOR_AUDIENCE
        | method::MEMORY_FOR_FILE
        | method::MEMORY_UNIFIED_SEARCH
        | method::MEMORY_GRAPH
        | method::MEMORY_STATS => {
            let viewer = Viewer {
                session_id: state.session.clone(),
            };
            let pid = project_id.expect("scoped");
            let m = req.method.clone();
            let params = obj(req.params)?;
            store_op(shared, pid, move |store| {
                let s = |k: &str| params.get(k).and_then(Value::as_str).map(str::to_string);
                let n = |k: &str| params.get(k).and_then(Value::as_u64).map(|v| v as usize);
                match m.as_str() {
                    method::MEMORY_LIST_PAGE => {
                        let statuses: Option<Vec<String>> = params
                            .get("statuses")
                            .map(|v| serde_json::from_value(v.clone()))
                            .transpose()
                            .map_err(|_| {
                                err(ErrorCode::InvalidRequest, "statuses must be an array")
                            })?;
                        to_value(store.list_page(
                            statuses.as_deref(),
                            s("kind").as_deref(),
                            s("query").as_deref(),
                            n("limit"),
                            s("cursor").as_deref(),
                            &viewer,
                        )?)
                    }
                    method::MEMORY_RELATED => {
                        let ids: Vec<String> =
                            serde_json::from_value(params.get("ids").cloned().unwrap_or(json!([])))
                                .map_err(|_| {
                                    err(
                                        ErrorCode::InvalidRequest,
                                        "ids must be an array of strings",
                                    )
                                })?;
                        let hits = store.find_related(
                            &ids,
                            n("limit").unwrap_or(10),
                            n("maxDepth").unwrap_or(3),
                            &viewer,
                        )?;
                        Ok(json!({ "hits": hits }))
                    }
                    method::MEMORY_FOR_AUDIENCE => {
                        // Only role/taskType/mode, as bounded strings: the
                        // context reaches the truncation audit record.
                        const FIELD_MAX: usize = 256;
                        let raw = params.get("context").cloned().unwrap_or(json!({}));
                        let Some(fields) = raw.as_object() else {
                            return Err(err(
                                ErrorCode::InvalidRequest,
                                "context must be an object",
                            ));
                        };
                        let mut context = serde_json::Map::new();
                        for (k, v) in fields {
                            if !matches!(k.as_str(), "role" | "taskType" | "mode") {
                                return Err(err(
                                    ErrorCode::InvalidRequest,
                                    "context accepts only role, taskType and mode",
                                ));
                            }
                            match v {
                                Value::Null => {}
                                Value::String(s) if s.len() <= FIELD_MAX => {
                                    context.insert(k.clone(), v.clone());
                                }
                                _ => {
                                    return Err(err(
                                        ErrorCode::InvalidRequest,
                                        format!(
                                            "context.{k} must be a string of at most {FIELD_MAX} bytes"
                                        ),
                                    ));
                                }
                            }
                        }
                        let context = Value::Object(context);
                        let limit = params.get("limit").and_then(Value::as_i64);
                        to_value(store.retrieve_for_audience(
                            &context,
                            limit,
                            &viewer,
                            &now_iso(),
                        )?)
                    }
                    method::MEMORY_FOR_FILE => {
                        let path = s("path")
                            .filter(|p| !p.trim().is_empty())
                            .ok_or_else(|| err(ErrorCode::InvalidRequest, "path is required"))?;
                        let int = |k: &str| params.get(k).and_then(Value::as_i64);
                        let q = wrongmemory_storage::find_file::FileQuery {
                            include_superseded: params
                                .get("includeSuperseded")
                                .and_then(Value::as_bool),
                            include_deleted: params.get("includeDeleted").and_then(Value::as_bool),
                            limit: int("limit"),
                            line_start: int("lineStart"),
                            line_end: int("lineEnd"),
                        };
                        store.find_memories_for_file(&path, &q, &viewer, &now_iso())
                    }
                    method::MEMORY_UNIFIED_SEARCH => {
                        // Session context comes from the connection; any
                        // client-supplied sessionId/includeAllSessions is
                        // ignored (dropped before parsing).
                        let query = params.get("query").cloned().unwrap_or(json!({}));
                        let mut options = params.get("options").cloned().unwrap_or(json!({}));
                        if let Some(o) = options.as_object_mut() {
                            o.remove("sessionId");
                            o.remove("includeAllSessions");
                        }
                        store.unified_search(&query, &options, &viewer, &now_iso())
                    }
                    method::MEMORY_GRAPH => to_value(store.graph_for(
                        &s("query").unwrap_or_default(),
                        n("maxDepth").unwrap_or(2),
                        n("limit").unwrap_or(100),
                        &viewer,
                    )?),
                    _ => store.stats(&viewer),
                }
            })
            .await
        }
        method::MAINTENANCE_STATUS
        | method::MAINTENANCE_CONFIGURE
        | method::MAINTENANCE_RUN
        | method::JOBS_LIST
        | method::JOBS_GET
        | method::JOBS_CANCEL => {
            maintenance(shared, state, &req.method, project_id, req.params).await
        }
        method::EVENTS_SINCE => {
            let p: EventsSinceParams = parse(req.params)?;
            let pid = project_id.expect("scoped");
            events_since(shared, state, pid, p).await
        }
        other => Err(err(
            ErrorCode::MethodNotFound,
            format!("unknown method {other}"),
        )),
    }
}

/// `events.since`: committed changes after `generation`, filtered by what
/// the caller may see. Hidden and absent records are indistinguishable:
/// an event about a record the viewer cannot read is simply omitted.
async fn events_since(
    shared: &Arc<Shared>,
    state: &Conn,
    pid: String,
    p: EventsSinceParams,
) -> Result<Value, ErrorBody> {
    let instance = shared.instance_id.clone();
    // The project must exist (and be visible) even when no events exist.
    let id = pid.clone();
    shared
        .blocking(move |s| s.reg(|r| r.get_project(&id)))
        .await?
        .ok_or_else(|| err(ErrorCode::ProjectNotFound, "project not found"))?;
    let foreign = p
        .daemon_instance_id
        .as_deref()
        .is_some_and(|d| d != instance);
    let since = shared.events.since(&pid, p.generation);
    if foreign || since.resync {
        return to_value(EventsSinceResult {
            daemon_instance_id: instance,
            generation: since.generation,
            resync: true,
            events: vec![],
        });
    }
    let session = state.session.clone();
    let mut candidates: Vec<events::Entry> = since
        .entries
        .into_iter()
        .filter(|e| match &e.owner_session {
            Some(owner) => session.as_deref() == Some(owner.as_str()),
            None => true,
        })
        .collect();
    let ids: Vec<String> = candidates
        .iter()
        .filter_map(|e| e.event.memory_id.clone())
        .collect();
    // Review events (accept/reject/resolve) carry no owner session, so the
    // candidate itself is checked: a session-private proposal's id must not
    // reach other sessions (WM-603).
    let cand_ids: Vec<String> = candidates
        .iter()
        .filter_map(|e| e.event.candidate_id.clone())
        .collect();
    if !ids.is_empty() || !cand_ids.is_empty() {
        let viewer = Viewer {
            session_id: session.clone(),
        };
        let (visible, visible_cands) = store_op(shared, pid, move |store| {
            let mut ok = std::collections::HashSet::new();
            for id in ids {
                if store.get(&id, &viewer)?.is_some() {
                    ok.insert(id);
                }
            }
            let mut ok_c = std::collections::HashSet::new();
            for id in cand_ids {
                if store.candidate_visible(&id, &viewer)? {
                    ok_c.insert(id);
                }
            }
            Ok((ok, ok_c))
        })
        .await?;
        candidates.retain(|e| {
            e.event
                .memory_id
                .as_ref()
                .is_none_or(|id| visible.contains(id))
                && e.event
                    .candidate_id
                    .as_ref()
                    .is_none_or(|id| visible_cands.contains(id))
        });
    }
    to_value(EventsSinceResult {
        daemon_instance_id: instance,
        generation: since.generation,
        resync: false,
        events: candidates.into_iter().map(|e| e.event).collect(),
    })
}

fn job_visible(state: &Conn, job: &JobInfo) -> bool {
    state
        .principal
        .grant
        .as_ref()
        .is_none_or(|g| g.projects.contains(&job.project_id))
}

/// `maintenance.*` and `jobs.*`.
async fn maintenance(
    shared: &Arc<Shared>,
    state: &Conn,
    m: &str,
    project_id: Option<String>,
    params: Value,
) -> Result<Value, ErrorBody> {
    let params = obj(params)?;
    let job_id = || {
        params
            .get("jobId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| err(ErrorCode::InvalidRequest, "jobId is required"))
    };
    // Resolve a scoped project and its schedule (writable projects only get
    // schedules; read-only stores are never maintained).
    let project = |pid: String| async move {
        shared
            .blocking(move |s| {
                let p = s
                    .reg(|r| r.get_project(&pid))?
                    .ok_or_else(|| err(ErrorCode::ProjectNotFound, "project not found"))?;
                if p.admission != Admission::ReadWrite {
                    return Err(err(
                        ErrorCode::ReadOnly,
                        "maintenance requires a writable store",
                    ));
                }
                let sched = s.reg(|r| r.ensure_schedule(&pid))?;
                Ok((p, sched))
            })
            .await
    };
    match m {
        method::MAINTENANCE_STATUS => {
            let pid = project_id.expect("scoped");
            let (_, schedule) = project(pid.clone()).await?;
            let running = shared.scheduler.running_job();
            let jobs = shared
                .blocking(move |s| s.reg(|r| r.list_jobs(Some(&pid), 10)))
                .await?;
            to_value(json!({
                "schedule": schedule,
                "recentJobs": jobs,
                "schedulerBusy": running.is_some(),
            }))
        }
        method::MAINTENANCE_CONFIGURE => {
            let pid = project_id.expect("scoped");
            let p: MaintenanceConfigureParams = parse(Value::Object(params))?;
            if let Some(c) = p.cadence_ms
                && c < 1000
            {
                return Err(err(ErrorCode::InvalidRequest, "cadenceMs must be >= 1000"));
            }
            if let Some(o) = &p.options {
                scheduler::validate_options(o)?;
            }
            project(pid.clone()).await?;
            let r = shared
                .blocking(move |s| {
                    s.reg(|r| {
                        r.configure_schedule(
                            &pid,
                            p.cadence_ms,
                            p.options.as_ref(),
                            p.enabled,
                            p.expected_options_revision,
                        )
                    })
                })
                .await?;
            match r {
                Ok(schedule) => to_value(json!({ "schedule": schedule })),
                Err(current) => Err(err(
                    ErrorCode::RevisionConflict,
                    format!(
                        "options revision is {}, not the expected revision",
                        current.options_revision
                    ),
                )),
            }
        }
        method::MAINTENANCE_RUN => {
            let pid = project_id.expect("scoped");
            let options = params.get("options").cloned();
            if let Some(o) = &options {
                scheduler::validate_options(o)?;
            }
            let (_, schedule) = project(pid.clone()).await?;
            let options = options.unwrap_or(schedule.options);
            let admitted = shared
                .blocking(move |s| {
                    s.reg(|r| {
                        r.admit_job(
                            &pid,
                            scheduler::KIND_HYGIENE,
                            scheduler::PRIORITY_MANUAL,
                            &options,
                        )
                    })
                })
                .await?;
            match admitted {
                JobAdmission::Queued(job) => to_value(json!({"job": job, "coalesced": false})),
                JobAdmission::Coalesced(job) => to_value(json!({"job": job, "coalesced": true})),
                JobAdmission::Saturated => Err(err(
                    ErrorCode::QueueSaturated,
                    "maintenance queue is full; retry later",
                )),
            }
        }
        method::JOBS_LIST => {
            let limit = params
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .min(MAX_PAGE_SIZE as u64) as usize;
            let jobs = shared
                .blocking(move |s| s.reg(|r| r.list_jobs(project_id.as_deref(), limit)))
                .await?;
            let jobs: Vec<_> = jobs.into_iter().filter(|j| job_visible(state, j)).collect();
            to_value(json!({ "jobs": jobs }))
        }
        method::JOBS_GET | method::JOBS_CANCEL => {
            let id = job_id()?;
            let lookup = id.clone();
            let job = shared
                .blocking(move |s| s.reg(|r| r.get_job(&lookup)))
                .await?
                .filter(|j| job_visible(state, j))
                .ok_or_else(|| err(ErrorCode::NotFound, "job not found"))?;
            if m == method::JOBS_GET {
                return to_value(job);
            }
            let canceled = shared
                .blocking(move |s| s.reg(|r| r.cancel_job(&id)))
                .await?;
            if !canceled {
                return Err(err(
                    ErrorCode::InvalidRequest,
                    format!("job is {}; only queued jobs can be canceled", job.state),
                ));
            }
            let job = shared
                .blocking(move |s| s.reg(|r| r.get_job(&job.job_id)))
                .await?;
            to_value(json!({ "canceled": true, "job": job }))
        }
        _ => Err(err(ErrorCode::MethodNotFound, "unknown method")),
    }
}
