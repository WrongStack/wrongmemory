//! Wire contracts shared by the WrongMemory daemon, CLI, MCP bridge, and the
//! future TypeScript WrongStack adapter.
//!
//! Framing (architecture §9): newline-delimited UTF-8 JSON. Every frame is one
//! JSON value followed by `\n`; compact JSON never contains a raw newline. The
//! first frame a client sends is an [`AuthRequest`]; nothing else is accepted
//! before it. This is neither the legacy SAGE project-server protocol nor MCP.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub mod record;

pub use record::*;

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 0;

/// Initial maximum request/response frame size (4 MiB, architecture §9).
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Authentication frames are tiny; never buffer a large unauthenticated payload.
pub const MAX_AUTH_FRAME_BYTES: usize = 16 * 1024;
/// Largest page any read operation returns.
pub const MAX_PAGE_SIZE: usize = 200;

/// SAGE SQLite storage profile this build understands. Resolves to
/// `contracts/profiles/sage-sqlite-v5-baseline/manifest.json`.
pub const STORAGE_PROFILE_ID: &str = "sage-sqlite-v5-baseline";
/// Additive WrongMemory extension (receipt table) applied only to stores the
/// service owns for writing. Readers that do not know it ignore the table.
pub const EXTENSION_PROFILE_ID: &str = "wrongmemory-ext-v1";
/// Retrieval/lifecycle behavior profile implemented natively. `slice` means
/// the M1 subset; it is NOT full SAGE semantic parity (see docs/status.md).
pub const SEMANTIC_PROFILE_ID: &str = "wrongmemory-native-slice-v0";

/// Exit codes shared by the CLI (validation-and-operations §8).
pub mod exit_code {
    pub const SUCCESS: i32 = 0;
    pub const FAILED: i32 = 1;
    pub const INVALID_INPUT: i32 = 2;
    pub const SERVICE_UNAVAILABLE: i32 = 3;
    pub const UNSUPPORTED: i32 = 4;
    pub const DENIED: i32 = 5;
    pub const CONFLICT: i32 = 6;
    pub const TIMEOUT: i32 = 7;
    pub const INTERNAL: i32 = 70;
}

/// Stable typed error codes. Serialized as SCREAMING_SNAKE_CASE strings.
///
/// Privacy rule: a hidden record and an absent record both produce
/// `NOT_FOUND`; an ungranted project and an unknown project both produce
/// `PROJECT_NOT_FOUND`. Error shape never discloses hidden existence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    Unauthenticated,
    Forbidden,
    ProtocolUnsupported,
    ProfileUnsupported,
    ProjectNotFound,
    NotFound,
    StoreIdentityConflict,
    OwnerConflict,
    ReadOnly,
    RevisionConflict,
    IdempotencyMismatch,
    InvalidRequest,
    MethodNotFound,
    NotImplemented,
    QueueSaturated,
    DatabaseBusy,
    DatabaseCorrupt,
    DiskFull,
    ShuttingDown,
    DeadlineExceeded,
    StoreUnavailable,
    Internal,
}

impl ErrorCode {
    /// Whether a client may retry the same request unchanged.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            ErrorCode::QueueSaturated
                | ErrorCode::DatabaseBusy
                | ErrorCode::DeadlineExceeded
                | ErrorCode::ShuttingDown
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Unauthenticated => "UNAUTHENTICATED",
            ErrorCode::Forbidden => "FORBIDDEN",
            ErrorCode::ProtocolUnsupported => "PROTOCOL_UNSUPPORTED",
            ErrorCode::ProfileUnsupported => "PROFILE_UNSUPPORTED",
            ErrorCode::ProjectNotFound => "PROJECT_NOT_FOUND",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::StoreIdentityConflict => "STORE_IDENTITY_CONFLICT",
            ErrorCode::OwnerConflict => "OWNER_CONFLICT",
            ErrorCode::ReadOnly => "READ_ONLY",
            ErrorCode::RevisionConflict => "REVISION_CONFLICT",
            ErrorCode::IdempotencyMismatch => "IDEMPOTENCY_MISMATCH",
            ErrorCode::InvalidRequest => "INVALID_REQUEST",
            ErrorCode::MethodNotFound => "METHOD_NOT_FOUND",
            ErrorCode::NotImplemented => "NOT_IMPLEMENTED",
            ErrorCode::QueueSaturated => "QUEUE_SATURATED",
            ErrorCode::DatabaseBusy => "DATABASE_BUSY",
            ErrorCode::DatabaseCorrupt => "DATABASE_CORRUPT",
            ErrorCode::DiskFull => "DISK_FULL",
            ErrorCode::ShuttingDown => "SHUTTING_DOWN",
            ErrorCode::DeadlineExceeded => "DEADLINE_EXCEEDED",
            ErrorCode::StoreUnavailable => "STORE_UNAVAILABLE",
            ErrorCode::Internal => "INTERNAL",
        }
    }

    /// CLI exit code for this error class.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::Unauthenticated | ErrorCode::Forbidden | ErrorCode::ReadOnly => {
                exit_code::DENIED
            }
            ErrorCode::ProtocolUnsupported
            | ErrorCode::ProfileUnsupported
            | ErrorCode::NotImplemented
            | ErrorCode::MethodNotFound => exit_code::UNSUPPORTED,
            ErrorCode::RevisionConflict
            | ErrorCode::IdempotencyMismatch
            | ErrorCode::OwnerConflict
            | ErrorCode::StoreIdentityConflict => exit_code::CONFLICT,
            ErrorCode::InvalidRequest | ErrorCode::ProjectNotFound | ErrorCode::NotFound => {
                exit_code::INVALID_INPUT
            }
            ErrorCode::ShuttingDown
            | ErrorCode::QueueSaturated
            | ErrorCode::DatabaseBusy
            | ErrorCode::StoreUnavailable => exit_code::SERVICE_UNAVAILABLE,
            ErrorCode::DeadlineExceeded => exit_code::TIMEOUT,
            ErrorCode::DatabaseCorrupt | ErrorCode::DiskFull => exit_code::FAILED,
            ErrorCode::Internal => exit_code::INTERNAL,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.retryable(),
        }
    }
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ErrorBody {}

// ─── Authorization model ────────────────────────────────────────────────────

/// Server-enforced action classes (mcp-and-client-integration §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Read,
    Propose,
    Write,
    Curate,
    Admin,
}

/// Policy profile carried by a grant. The daemon derives allowed actions from
/// it; a client cannot widen it by sending a different action or label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyProfile {
    ReadOnly,
    ReadPropose,
    Writer,
    Curator,
}

impl PolicyProfile {
    pub fn allows(self, action: Action) -> bool {
        match (self, action) {
            (_, Action::Admin) => false,
            (_, Action::Read) => true,
            (PolicyProfile::ReadOnly, _) => false,
            (PolicyProfile::ReadPropose, Action::Propose) => true,
            (PolicyProfile::ReadPropose, _) => false,
            (PolicyProfile::Writer, Action::Propose | Action::Write) => true,
            (PolicyProfile::Writer, _) => false,
            (PolicyProfile::Curator, _) => true,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PolicyProfile::ReadOnly => "read-only",
            PolicyProfile::ReadPropose => "read-propose",
            PolicyProfile::Writer => "writer",
            PolicyProfile::Curator => "curator",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read-only" => Some(PolicyProfile::ReadOnly),
            "read-propose" => Some(PolicyProfile::ReadPropose),
            "writer" => Some(PolicyProfile::Writer),
            "curator" => Some(PolicyProfile::Curator),
            _ => None,
        }
    }
}

/// How a client authenticates. `host` is the owner-private daemon credential
/// used by the local CLI and trusted WrongStack hosts; it must never be put in
/// a model-facing client configuration (WM-ADR-005). `grant` is a scoped
/// external-client credential stored under the private data root.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum Credential {
    Host { token: String },
    Grant { grant_id: String, secret: String },
}

// ─── Handshake ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthRequest {
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub credential: Credential,
    /// Attributed label only; never proof of identity.
    pub client_name: String,
    pub client_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_frame_bytes: usize,
    pub max_page_size: usize,
    pub max_inflight: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrincipalInfo {
    pub principal_id: String,
    /// `host` or `grant`.
    pub kind: String,
    /// `None` for the trusted host principal.
    pub profile: Option<PolicyProfile>,
    /// `None` means every registered project (host principal only).
    pub projects: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub daemon_instance_id: String,
    pub daemon_version: String,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub storage_profile_id: String,
    pub extension_profile_id: String,
    pub semantic_profile_id: String,
    pub capabilities: Vec<String>,
    pub limits: Limits,
    pub principal: PrincipalInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum AuthResponse {
    Ok { hello: Box<Hello> },
    Error { error: ErrorBody },
}

// ─── Request / response envelope ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub protocol: u32,
    pub request_id: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default)]
    pub params: Value,
    /// Durable mutation retry identity, scoped to principal + project + action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(request_id: impl Into<String>, result: Value) -> Self {
        Self {
            request_id: request_id.into(),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(request_id: impl Into<String>, error: ErrorBody) -> Self {
        Self {
            request_id: request_id.into(),
            result: None,
            error: Some(error),
        }
    }
}

/// Method names (one list, so the TS adapter can be generated from it).
pub mod method {
    pub const DAEMON_HEALTH: &str = "daemon.health";
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
    pub const PROJECT_REGISTER: &str = "project.register";
    pub const PROJECT_LIST: &str = "project.list";
    pub const PROJECT_GET: &str = "project.get";
    pub const PROJECT_DETACH: &str = "project.detach";
    pub const GRANT_CREATE: &str = "grant.create";
    pub const GRANT_LIST: &str = "grant.list";
    pub const GRANT_REVOKE: &str = "grant.revoke";
    pub const SESSION_ACQUIRE: &str = "session.acquire";
    pub const SESSION_RELEASE: &str = "session.release";
    pub const MEMORY_SEARCH: &str = "memory.search";
    pub const MEMORY_GET: &str = "memory.get";
    pub const MEMORY_FOR_PATH: &str = "memory.forPath";
    pub const MEMORY_REMEMBER: &str = "memory.remember";
    pub const CANDIDATE_LIST: &str = "candidate.list";
    pub const CANDIDATE_PROPOSE: &str = "candidate.propose";
    pub const CANDIDATE_ACCEPT: &str = "candidate.accept";
    pub const CANDIDATE_REJECT: &str = "candidate.reject";
    pub const CANDIDATE_RESOLVE: &str = "candidate.resolve";
    pub const MEMORY_UPDATE: &str = "memory.update";
    pub const MEMORY_FEEDBACK: &str = "memory.feedback";
    pub const MEMORY_DELETE: &str = "memory.delete";
    pub const MEMORY_RECORD_USAGE: &str = "memory.recordUsage";
    pub const MEMORY_LIST_PAGE: &str = "memory.listPage";
    pub const MEMORY_RELATED: &str = "memory.related";
    pub const MEMORY_FOR_AUDIENCE: &str = "memory.forAudience";
    pub const MEMORY_FOR_FILE: &str = "memory.forFile";
    pub const MEMORY_UNIFIED_SEARCH: &str = "memory.unifiedSearch";
    pub const MEMORY_GRAPH: &str = "memory.graph";
    pub const MEMORY_STATS: &str = "memory.stats";
    pub const MEMORY_RECOVER: &str = "memory.recover";
    pub const MEMORY_BACKFILL: &str = "memory.backfill";
    pub const MEMORY_HYGIENE: &str = "memory.hygiene";
    pub const MEMORY_VERIFY: &str = "memory.verify";
    pub const MEMORY_IMPORT_LEGACY: &str = "memory.importLegacy";
    /// Legacy `MemoryStore` bulk mutations (curator only).
    pub const MEMORY_LEGACY_FORGET: &str = "memory.legacyForget";
    pub const MEMORY_LEGACY_CONSOLIDATE: &str = "memory.legacyConsolidate";
    pub const MEMORY_LEGACY_CLEAR: &str = "memory.legacyClear";
    pub const MAINTENANCE_STATUS: &str = "maintenance.status";
    pub const MAINTENANCE_CONFIGURE: &str = "maintenance.configure";
    pub const MAINTENANCE_RUN: &str = "maintenance.run";
    pub const JOBS_LIST: &str = "jobs.list";
    pub const JOBS_GET: &str = "jobs.get";
    pub const JOBS_CANCEL: &str = "jobs.cancel";
    pub const EVENTS_SINCE: &str = "events.since";

    /// Every method, in a stable order.
    pub const ALL: &[&str] = &[
        DAEMON_HEALTH,
        DAEMON_SHUTDOWN,
        PROJECT_REGISTER,
        PROJECT_LIST,
        PROJECT_GET,
        PROJECT_DETACH,
        GRANT_CREATE,
        GRANT_LIST,
        GRANT_REVOKE,
        SESSION_ACQUIRE,
        SESSION_RELEASE,
        MEMORY_SEARCH,
        MEMORY_GET,
        MEMORY_FOR_PATH,
        MEMORY_REMEMBER,
        CANDIDATE_LIST,
        CANDIDATE_PROPOSE,
        CANDIDATE_ACCEPT,
        CANDIDATE_REJECT,
        CANDIDATE_RESOLVE,
        MEMORY_UPDATE,
        MEMORY_FEEDBACK,
        MEMORY_DELETE,
        MEMORY_RECORD_USAGE,
        MEMORY_LIST_PAGE,
        MEMORY_RELATED,
        MEMORY_FOR_AUDIENCE,
        MEMORY_FOR_FILE,
        MEMORY_UNIFIED_SEARCH,
        MEMORY_GRAPH,
        MEMORY_STATS,
        MEMORY_RECOVER,
        MEMORY_BACKFILL,
        MEMORY_HYGIENE,
        MEMORY_VERIFY,
        MEMORY_IMPORT_LEGACY,
        MEMORY_LEGACY_FORGET,
        MEMORY_LEGACY_CONSOLIDATE,
        MEMORY_LEGACY_CLEAR,
        MAINTENANCE_STATUS,
        MAINTENANCE_CONFIGURE,
        MAINTENANCE_RUN,
        JOBS_LIST,
        JOBS_GET,
        JOBS_CANCEL,
        EVENTS_SINCE,
    ];
}

/// Discovery metadata published in `runtime/endpoint.json` (no secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointInfo {
    pub endpoint: String,
    pub daemon_instance_id: String,
    pub daemon_version: String,
    pub pid: u32,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub started_at_ms: i64,
}

// ─── Service DTOs ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub daemon_instance_id: String,
    pub daemon_version: String,
    pub pid: u32,
    pub uptime_ms: i64,
    pub data_root: String,
    pub sqlite_version: String,
    pub projects: usize,
    pub open_stores: usize,
    pub connections: usize,
    pub draining: bool,
}

/// How a project's authoritative store was attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreKind {
    /// Existing `<root>/.wrongstack/memories/sage.db`, kept in place.
    Adopted,
    /// Service-managed out-of-tree store under `<data-root>/projects/<id>/`.
    Managed,
}

/// Effective admission of a store after read-only inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Admission {
    /// Service owns the store (owner guard held) and may mutate it.
    ReadWrite,
    /// Readable only: a legacy owner may be active, write was not requested,
    /// or the profile is not writable by this build.
    ReadOnly,
    /// Not readable by this build (unknown/newer profile or unreadable).
    Refused,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterProjectParams {
    /// Project root on disk. For `adopted`, the store is resolved under it.
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub store_kind: StoreKind,
    /// Request write ownership of an adopted store. Refused while a legacy
    /// SAGE owner may be active (`server.json` present) unless `force_owner`.
    #[serde(default)]
    pub write: bool,
    /// Operator acknowledgement that the legacy SAGE owner described by
    /// `server.json` is stopped. Never bypasses the OS owner guard, and an
    /// unmodified SAGE process ignores that guard, so use it only when the
    /// legacy daemon is really down.
    #[serde(default)]
    pub force_owner: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub project_id: String,
    pub display_name: String,
    pub root: String,
    pub store_path: String,
    pub store_kind: StoreKind,
    pub admission: Admission,
    pub admission_reason: String,
    pub owner_epoch: i64,
    pub storage_profile_id: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterProjectResult {
    pub project: ProjectInfo,
    /// False when the same root/store was already registered (idempotent).
    pub created: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantCreateParams {
    pub label: String,
    pub profile: PolicyProfile,
    pub projects: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantInfo {
    pub grant_id: String,
    pub label: String,
    pub profile: PolicyProfile,
    pub projects: Vec<String>,
    pub created_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub revoked_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantCreateResult {
    pub grant: GrantInfo,
    /// Returned once to the trusted caller, which stores it owner-private.
    pub secret: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAcquireParams {
    /// Trusted hosts may bind an existing host session ID (e.g. a WrongStack
    /// agent session). Grant principals always receive a fresh daemon-issued
    /// session and cannot name one (WM-INV-03).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: String,
    pub daemon_issued: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchParams {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    /// Defaults to `["active"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statuses: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForPathParams {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub memory: SageRecord,
    /// Channel that produced the hit: `fts`, `like`, `path`.
    pub channel: String,
    /// Channel-relative score; not comparable across channels.
    pub score: f64,
    /// Short structural reasons (matched anchor, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
    /// Rows that matched but could not be decoded (corruption is reported,
    /// valid siblings still return).
    pub corrupt_skipped: usize,
    pub channel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetParams {
    pub id: String,
}

/// Input accepted by `memory.remember`. Ownership of session-scoped memory is
/// taken from the bound session, never from a parameter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberParams {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub anchors: Vec<MemoryAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<MemoryAudience>,
    #[serde(default)]
    pub sources: Vec<MemorySource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validity: Option<MemoryValidity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub importance: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contradicts: Vec<String>,
}

/// Input accepted by `candidate.propose` (a proposal, not a memory).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposeParams {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub anchors: Vec<MemoryAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<MemoryAudience>,
    #[serde(default)]
    pub sources: Vec<MemorySource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validity: Option<MemoryValidity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub importance: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_memory_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateListParams {
    /// Defaults to `pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateListResult {
    pub candidates: Vec<MemoryCandidate>,
    pub truncated: bool,
    pub corrupt_skipped: usize,
}

/// Outcome of an admitted mutation. Distinguishes a pending proposal from a
/// committed authoritative record (mcp-and-client-integration §5.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationOutcome {
    /// `memory-committed` or `candidate-pending`.
    pub outcome: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
    /// True when this reply was served from a durable receipt (retry).
    pub replayed: bool,
    pub record: Value,
}

// ─── Maintenance, jobs, events (M2) ─────────────────────────────────────────

/// Durable per-project maintenance schedule (registry `maintenance_schedules`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleInfo {
    pub project_id: String,
    pub enabled: bool,
    pub cadence_ms: i64,
    /// Hygiene options (`HygieneOptions` JSON, camelCase).
    pub options: Value,
    pub options_revision: i64,
    pub next_due_ms: i64,
    pub last_run_ms: Option<i64>,
    pub last_outcome: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceConfigureParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cadence_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Optimistic guard: the options revision the caller read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_options_revision: Option<i64>,
}

/// Durable job record (registry `jobs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInfo {
    pub job_id: String,
    pub project_id: String,
    /// `hygiene`.
    pub kind: String,
    /// `queued`, `running`, `succeeded`, `failed`, `canceled`.
    pub state: String,
    pub priority: i64,
    pub options: Value,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub attempt: i64,
}

/// One committed change. Carries ids and revisions only, never memory text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeEvent {
    pub generation: u64,
    /// Method that committed the change (e.g. `memory.update`), or
    /// `maintenance.hygiene` for scheduled passes.
    pub change: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
    /// Usage counters only; does not invalidate content caches.
    #[serde(default)]
    pub telemetry: bool,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsSinceParams {
    pub generation: u64,
    /// Daemon instance the caller's generation belongs to; a different
    /// instance always forces a resync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_instance_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsSinceResult {
    pub daemon_instance_id: String,
    pub generation: u64,
    /// True when the requested range is no longer available: reload state.
    pub resync: bool,
    pub events: Vec<ChangeEvent>,
}

/// Convenience: build a JSON object from key/value pairs.
pub fn object(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn profiles_gate_actions() {
        use Action::*;
        use PolicyProfile::*;
        assert!(ReadOnly.allows(Read) && !ReadOnly.allows(Propose));
        assert!(ReadPropose.allows(Propose) && !ReadPropose.allows(Write));
        assert!(Writer.allows(Write) && !Writer.allows(Curate));
        assert!(Curator.allows(Curate));
        for p in [ReadOnly, ReadPropose, Writer, Curator] {
            assert!(!p.allows(Admin), "{p:?} must never grant admin");
            assert_eq!(PolicyProfile::parse(p.as_str()), Some(p));
        }
    }

    #[test]
    fn error_code_wire_names_match_as_str() {
        for code in [
            ErrorCode::NotFound,
            ErrorCode::ProjectNotFound,
            ErrorCode::RevisionConflict,
            ErrorCode::IdempotencyMismatch,
            ErrorCode::ProfileUnsupported,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
        }
    }

    #[test]
    fn credentials_are_tagged() {
        let c: Credential =
            serde_json::from_value(json!({"kind":"grant","grantId":"g1","secret":"s"})).unwrap();
        assert!(matches!(c, Credential::Grant { .. }));
        let c: Credential = serde_json::from_value(json!({"kind":"host","token":"t"})).unwrap();
        assert!(matches!(c, Credential::Host { .. }));
    }

    #[test]
    fn methods_are_unique() {
        let mut v = method::ALL.to_vec();
        v.sort();
        v.dedup();
        assert_eq!(v.len(), method::ALL.len());
    }
}
