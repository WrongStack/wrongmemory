//! Project registration, store admission, and per-project store handles.
//!
//! Each project has at most one open [`SageStore`] behind a mutex: the mutex
//! is the project's serialized writer lane, and SQLite work always runs on the
//! blocking pool. Writable handles hold the store's [`OwnerGuard`] and an
//! owner epoch recorded in the registry (WM-INV-01).
//!
//! Open handles are capped (default 32). Opening one more evicts the least
//! recently used idle handle: no request holds it and none is waiting for
//! its lane. Eviction closes SQLite, releases the owner guard, and journals
//! the release. A handle whose store failed (missing file, disk full, I/O)
//! is dropped so a later request reopens it after recovery (WM-208); an
//! adopted store is never recreated.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use wrongmemory_contracts::{
    Admission, ErrorBody, ErrorCode, ProjectInfo, RegisterProjectParams, RegisterProjectResult,
    STORAGE_PROFILE_ID, StoreKind,
};
use wrongmemory_ipc::DataRoot;
use wrongmemory_registry::{NewProject, Registry, now_iso, now_ms};
use wrongmemory_storage::{OwnerGuard, SageStore, inspect};

/// Default cap on simultaneously open stores.
pub const DEFAULT_MAX_OPEN_STORES: usize = 32;

pub struct ProjectHandle {
    pub info: ProjectInfo,
    pub store: Mutex<SageStore>,
    _guard: Option<OwnerGuard>,
    /// Requests currently waiting for or holding this project's lane.
    waiting: AtomicUsize,
    last_used_ms: AtomicI64,
    /// Set once the project was exported and detached: a request that was
    /// already waiting for the lane must not write to the retired store.
    retired: AtomicBool,
}

/// Holds the project lane; decrements the waiter count on drop.
pub struct Lane<'a> {
    guard: MutexGuard<'a, SageStore>,
    waiting: &'a AtomicUsize,
}

impl std::ops::Deref for Lane<'_> {
    type Target = SageStore;
    fn deref(&self) -> &SageStore {
        &self.guard
    }
}

impl std::ops::DerefMut for Lane<'_> {
    fn deref_mut(&mut self) -> &mut SageStore {
        &mut self.guard
    }
}

impl Drop for Lane<'_> {
    fn drop(&mut self) {
        self.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ProjectHandle {
    /// Interactive acquisition of the project lane (counted as waiting so
    /// maintenance yields to it).
    pub fn lock(&self) -> Result<Lane<'_>, ErrorBody> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        self.last_used_ms.store(now_ms(), Ordering::Relaxed);
        match self.store.lock() {
            Ok(_) if self.retired.load(Ordering::SeqCst) => {
                self.waiting.fetch_sub(1, Ordering::SeqCst);
                Err(ErrorBody::new(
                    ErrorCode::ProjectNotFound,
                    "project not found",
                ))
            }
            Ok(guard) => Ok(Lane {
                guard,
                waiting: &self.waiting,
            }),
            Err(_) => {
                self.waiting.fetch_sub(1, Ordering::SeqCst);
                Err(internal("store lock poisoned"))
            }
        }
    }

    /// Refuse every later acquisition of this handle's lane.
    pub fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
    }

    /// Whether the project was exported and detached under this handle.
    /// Callers that take `store` directly (maintenance) must check it after
    /// acquiring the lock.
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    /// Requests waiting for or holding the lane.
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    pub fn writable(&self) -> bool {
        self._guard.is_some()
    }
}

pub struct Stores {
    open: Mutex<HashMap<String, Arc<ProjectHandle>>>,
    /// Serializes the miss path (re-check, evict, open, insert). Without it
    /// two requests for a closed project both opened it and the second lost
    /// the owner lock to the first: OWNER_CONFLICT against this very daemon
    /// (found by the WM-604 soak). Cache hits never take it.
    opening: Mutex<()>,
    /// The daemon's data root, recorded in the adoption marker of an adopted
    /// store so a host knows which WrongMemory serves it.
    data_root: PathBuf,
    max_open: usize,
}

fn internal(e: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, e.to_string())
}

/// Errors after which a cached handle must not be reused.
pub fn invalidates_handle(e: &ErrorBody) -> bool {
    matches!(
        e.code,
        ErrorCode::StoreUnavailable | ErrorCode::DiskFull | ErrorCode::DatabaseCorrupt
    ) || (e.code == ErrorCode::Internal
        && (e.message.contains("disk I/O error") || e.message.contains("unable to open")))
}

/// Canonical absolute directory. Strips the Windows verbatim prefix so the
/// stored display path stays usable; identity keys are derived separately.
pub fn canonical_dir(p: &Path) -> Result<PathBuf, ErrorBody> {
    let c = std::fs::canonicalize(p).map_err(|e| {
        ErrorBody::new(
            ErrorCode::InvalidRequest,
            format!("cannot resolve {}: {e}", p.display()),
        )
    })?;
    if !c.is_dir() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidRequest,
            format!("{} is not a directory", c.display()),
        ));
    }
    let s = c.to_string_lossy();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => c,
    })
}

/// WM-INV-14: the checkout a host connection says it works in. `Ok(None)` is
/// the project's own root. `Ok(Some(root))` is a linked worktree of it, with
/// Git's link checked from both sides (`linked_worktree_main`), so an
/// unrelated directory cannot borrow the project's evidence. Anything else is
/// refused. Network and device paths are refused before any filesystem call.
pub fn resolve_checkout(project_root: &str, requested: &str) -> Result<Option<PathBuf>, ErrorBody> {
    let path = Path::new(requested);
    if requested.starts_with("\\\\") || requested.starts_with("//") || !path.is_absolute() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidRequest,
            "checkout root must be an absolute local path",
        ));
    }
    let checkout = canonical_dir(path)?;
    let main = canonical_dir(Path::new(project_root))?;
    if checkout == main {
        return Ok(None);
    }
    let linked_main = wrongmemory_storage::wrongstack_paths::linked_worktree_main(&checkout)
        .and_then(|m| canonical_dir(&m).ok());
    if linked_main.as_ref() == Some(&main) {
        return Ok(Some(checkout));
    }
    Err(ErrorBody::new(
        ErrorCode::Forbidden,
        "checkout is neither the project root nor one of its linked worktrees",
    ))
}

/// SAGE's default project-local store location.
pub fn adopted_store_path(root: &Path) -> PathBuf {
    root.join(".wrongstack").join("memories").join("sage.db")
}

impl Stores {
    pub fn new(max_open: usize, data_root: PathBuf) -> Self {
        Self {
            open: Mutex::new(HashMap::new()),
            opening: Mutex::new(()),
            max_open: max_open.max(1),
            data_root,
        }
    }

    pub fn count(&self) -> usize {
        self.open.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Existing handle without opening one.
    pub fn peek(&self, project_id: &str) -> Option<Arc<ProjectHandle>> {
        self.open.lock().ok()?.get(project_id).cloned()
    }

    /// Open (or reuse) the handle for a registered project. Blocking.
    pub fn handle(
        &self,
        registry: &Mutex<Registry>,
        instance_id: &str,
        project_id: &str,
    ) -> Result<Arc<ProjectHandle>, ErrorBody> {
        if let Some(h) = self.open.lock().map_err(internal)?.get(project_id) {
            h.last_used_ms.store(now_ms(), Ordering::Relaxed);
            return Ok(h.clone());
        }
        let _opening = self.opening.lock().map_err(internal)?;
        // Re-check: another request may have opened it while we waited.
        if let Some(h) = self.open.lock().map_err(internal)?.get(project_id) {
            h.last_used_ms.store(now_ms(), Ordering::Relaxed);
            return Ok(h.clone());
        }
        let info = registry
            .lock()
            .map_err(internal)?
            .get_project(project_id)
            .map_err(ErrorBody::from)?
            .ok_or_else(|| ErrorBody::new(ErrorCode::ProjectNotFound, "project not found"))?;
        // Make room first so the owner guard of an evicted store is released
        // before another may be acquired.
        self.evict_to(self.max_open - 1, registry, instance_id)?;
        let handle = Arc::new(open_handle(registry, instance_id, info, &self.data_root)?);
        let mut open = self.open.lock().map_err(internal)?;
        Ok(open.entry(project_id.to_string()).or_insert(handle).clone())
    }

    /// Evict idle least-recently-used handles until at most `target` remain.
    /// Busy handles (in use or awaited) are never evicted; the cap is then
    /// temporarily exceeded rather than blocking.
    fn evict_to(
        &self,
        target: usize,
        registry: &Mutex<Registry>,
        instance_id: &str,
    ) -> Result<(), ErrorBody> {
        let mut evicted = Vec::new();
        {
            let mut open = self.open.lock().map_err(internal)?;
            while open.len() > target {
                let victim = open
                    .iter()
                    .filter(|(_, h)| Arc::strong_count(h) == 1 && h.waiting() == 0)
                    .min_by_key(|(_, h)| h.last_used_ms.load(Ordering::Relaxed))
                    .map(|(k, _)| k.clone());
                match victim.and_then(|k| open.remove(&k)) {
                    Some(h) => evicted.push(h),
                    None => break,
                }
            }
        }
        for h in evicted {
            release(h, registry, instance_id, "evicted");
        }
        Ok(())
    }

    /// Drop a handle (failure or detach) and journal its release.
    pub fn invalidate(
        &self,
        project_id: &str,
        registry: &Mutex<Registry>,
        instance_id: &str,
        why: &str,
    ) {
        if let Some(h) = self.open.lock().ok().and_then(|mut m| m.remove(project_id)) {
            release(h, registry, instance_id, why);
        }
    }

    pub fn drain_all(&self) -> Vec<Arc<ProjectHandle>> {
        self.open
            .lock()
            .map(|mut m| m.drain().map(|(_, v)| v).collect())
            .unwrap_or_default()
    }
}

/// Close a handle (SQLite first, then the owner guard) and journal it.
pub fn release(h: Arc<ProjectHandle>, registry: &Mutex<Registry>, instance_id: &str, why: &str) {
    let id = h.info.project_id.clone();
    let epoch = h.info.owner_epoch;
    let writable = h.writable();
    drop(h);
    if writable && let Ok(mut r) = registry.lock() {
        let _ = r.record_release(&id, epoch, instance_id);
    }
    crate::info!("store_released", {"projectId": id, "reason": why});
}

/// Longest wait for a predecessor daemon to release a store's owner lock:
/// the default drain window (15 s) plus release time.
const PREDECESSOR_RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

fn missing_store(path: &Path) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::StoreUnavailable,
        format!(
            "store {} is missing or unreachable; it is never recreated automatically",
            path.display()
        ),
    )
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    wrongmemory_ipc::win::process_alive(pid)
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    wrongmemory_ipc::unix::process_alive(pid)
}

/// Admission reason of an adopted store held read-only because a legacy SAGE
/// project server may own it. Such a registration is upgraded once that
/// server is gone (see `register`); registrations are recognized by the
/// prefix, which every daemon version has written.
const LEGACY_OWNER_REASON_PREFIX: &str = "legacy SAGE project-server metadata (server.json)";
const LEGACY_OWNER_REASON: &str = "legacy SAGE project-server metadata (server.json) present; an \
     unmodified SAGE owner ignores WrongMemory's owner guard. Stop it and re-register with \
     force_owner to acknowledge.";

/// Whether the SAGE project server named by the `server.json` next to `store`
/// may still run. Metadata whose pid no longer runs is stale (the server
/// exited without cleaning up); unreadable metadata counts as running.
fn legacy_server_may_run(store: &Path) -> bool {
    match wrongmemory_storage::owner::legacy_server_pid(store) {
        Some(pid) => pid != std::process::id() && process_alive(pid),
        None => true,
    }
}

fn open_handle(
    registry: &Mutex<Registry>,
    instance_id: &str,
    info: ProjectInfo,
    data_root: &Path,
) -> Result<ProjectHandle, ErrorBody> {
    let store_path = PathBuf::from(&info.store_path);
    let root = PathBuf::from(&info.root);
    if info.admission != Admission::Refused && !store_path.is_file() {
        return Err(missing_store(&store_path));
    }
    let handle = |store, guard| ProjectHandle {
        info: info.clone(),
        store: Mutex::new(store),
        _guard: guard,
        waiting: AtomicUsize::new(0),
        last_used_ms: AtomicI64::new(now_ms()),
        retired: AtomicBool::new(false),
    };
    match info.admission {
        Admission::Refused => Err(ErrorBody::new(
            ErrorCode::ProfileUnsupported,
            format!("store admission refused: {}", info.admission_reason),
        )),
        Admission::ReadOnly => {
            let (store, _) = SageStore::open(&store_path, false, Some(root), &now_iso())?;
            Ok(handle(store, None))
        }
        Admission::ReadWrite => {
            // An unmodified SAGE project server ignores the owner lock and
            // the adoption marker. While one runs on this store, writing
            // next to it would make WrongMemory the second writer.
            if info.store_kind == StoreKind::Adopted
                && let Some(pid) = wrongmemory_storage::owner::legacy_server_pid(&store_path)
                && pid != std::process::id()
                && process_alive(pid)
            {
                return Err(ErrorBody::new(
                    ErrorCode::OwnerConflict,
                    format!(
                        "a WrongStack SAGE project server (pid {pid}, server.json) has this store \
                         open; WrongMemory will not write next to it. Close that WrongStack \
                         session and retry, or delete server.json if no such server runs"
                    ),
                ));
            }
            let epoch_next = info.owner_epoch + 1;
            // A restart can elect this daemon while its predecessor (same
            // data root) is still draining and holds the owner lock. Wait,
            // bounded, for that release; any other holder fails at once.
            let deadline = std::time::Instant::now() + PREDECESSOR_RELEASE_WAIT;
            let guard = loop {
                match OwnerGuard::acquire(&store_path, epoch_next, instance_id) {
                    Ok(g) => break g,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Err(missing_store(&store_path));
                    }
                    Err(e) => {
                        let predecessor = e.kind() == std::io::ErrorKind::WouldBlock
                            && registry
                                .lock()
                                .map_err(internal)?
                                .predecessor_holds(&info.project_id, instance_id)
                                .unwrap_or(false);
                        if !predecessor || std::time::Instant::now() >= deadline {
                            return Err(ErrorBody::new(
                                ErrorCode::OwnerConflict,
                                format!("cannot acquire store ownership: {e}"),
                            ));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            };
            let (store, _) = SageStore::open(&store_path, true, Some(root), &now_iso())?;
            if info.store_kind == StoreKind::Adopted
                && let Err(e) = wrongmemory_storage::owner::write_adoption_marker(
                    &store_path,
                    &info.project_id,
                    data_root,
                )
            {
                // The lock still guards the store while this daemon runs.
                crate::warn!("adoption_marker_failed", {
                    "projectId": info.project_id,
                    "error": e.to_string(),
                });
            }
            let epoch = registry
                .lock()
                .map_err(internal)?
                .begin_ownership(&info.project_id, instance_id)
                .map_err(ErrorBody::from)?;
            let mut h = handle(store, Some(guard));
            h.info.owner_epoch = epoch;
            Ok(h)
        }
    }
}

/// `project.register`. Blocking. The flag is true when an existing
/// registration was upgraded to read-write; the caller then drops any
/// read-only handle it holds for that project.
pub fn register(
    data_root: &DataRoot,
    registry: &Mutex<Registry>,
    p: &RegisterProjectParams,
) -> Result<(RegisterProjectResult, bool), ErrorBody> {
    let root = canonical_dir(Path::new(&p.root))?;
    // Named after the root the project is registered under (the main
    // checkout when `p.root` is a linked worktree).
    let display_for = |root: &Path| {
        p.display_name.clone().unwrap_or_else(|| {
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string())
        })
    };
    let mut reg = registry.lock().map_err(internal)?;
    match p.store_kind {
        StoreKind::Adopted => {
            // SAGE resolves the store from the canonical repository root, so a
            // linked worktree shares its main checkout's store (WM-INV-14).
            // The worktree link is verified from both sides; a planted `.git`
            // file keeps its directory's own identity.
            let root = match wrongmemory_storage::wrongstack_paths::linked_worktree_main(&root) {
                Some(main) => canonical_dir(&main)?,
                None => root,
            };
            let display = display_for(&root);
            let store = adopted_store_path(&root);
            if let Some(existing) = reg.find_project_by_store(&store).map_err(ErrorBody::from)? {
                // Held read-only only because a legacy SAGE server might own
                // the store: a write request upgrades it once that server is
                // gone, or when the operator acknowledges it (`force_owner`).
                // The open-time check still refuses a writable open while the
                // server named in `server.json` runs.
                if p.write
                    && existing.admission == Admission::ReadOnly
                    && existing
                        .admission_reason
                        .starts_with(LEGACY_OWNER_REASON_PREFIX)
                    && (p.force_owner || !legacy_server_may_run(&store))
                    && inspect(&store).verdict.writable()
                {
                    reg.set_admission(
                        &existing.project_id,
                        Admission::ReadWrite,
                        "service owner",
                        existing.storage_profile_id.as_deref(),
                    )
                    .map_err(ErrorBody::from)?;
                    let project = reg
                        .get_project(&existing.project_id)
                        .map_err(ErrorBody::from)?
                        .ok_or_else(|| {
                            ErrorBody::new(ErrorCode::ProjectNotFound, "project not found")
                        })?;
                    return Ok((
                        RegisterProjectResult {
                            project,
                            created: false,
                        },
                        true,
                    ));
                }
                return Ok((
                    RegisterProjectResult {
                        project: existing,
                        created: false,
                    },
                    false,
                ));
            }
            if let Some(other) = reg.find_project_by_root(&root).map_err(ErrorBody::from)? {
                return Err(ErrorBody::new(
                    ErrorCode::StoreIdentityConflict,
                    format!(
                        "{} is already registered as {} with store {}; refusing a second authoritative store",
                        root.display(),
                        other.project_id,
                        other.store_path
                    ),
                ));
            }
            let insp = inspect(&store);
            if !insp.verdict.readable() {
                return Err(ErrorBody::new(
                    ErrorCode::ProfileUnsupported,
                    format!("{:?}: {}", insp.verdict, insp.detail),
                ));
            }
            let (admission, reason) = if !p.write {
                (
                    Admission::ReadOnly,
                    "read-only adoption (write not requested)".to_string(),
                )
            } else if !insp.verdict.writable() {
                (
                    Admission::ReadOnly,
                    format!("store not writable by this build: {}", insp.detail),
                )
            } else if insp.legacy_owner_metadata && !p.force_owner && legacy_server_may_run(&store)
            {
                (Admission::ReadOnly, LEGACY_OWNER_REASON.to_string())
            } else {
                (Admission::ReadWrite, "service owner".to_string())
            };
            let info = reg
                .insert_project(&NewProject {
                    display_name: &display,
                    root: &root,
                    store_path: &store,
                    store_kind: StoreKind::Adopted,
                    want_write: p.write,
                    admission,
                    admission_reason: &reason,
                    storage_profile_id: Some(STORAGE_PROFILE_ID),
                })
                .map_err(ErrorBody::from)?;
            Ok((
                RegisterProjectResult {
                    project: info,
                    created: true,
                },
                false,
            ))
        }
        StoreKind::Managed => {
            // A linked worktree shares its main checkout's memory identity
            // (WM-INV-14), as SAGE does. A host that looks for a project-local
            // store only in the worktree asks for `managed` (WrongStack
            // 1.4.0): answer with the main checkout's project, whatever its
            // store kind, and never create a second memory for the worktree.
            let root = match wrongmemory_storage::wrongstack_paths::linked_worktree_main(&root) {
                Some(main) => {
                    let main = canonical_dir(&main)?;
                    if let Some(existing) =
                        reg.find_project_by_root(&main).map_err(ErrorBody::from)?
                    {
                        return Ok((
                            RegisterProjectResult {
                                project: existing,
                                created: false,
                            },
                            false,
                        ));
                    }
                    main
                }
                None => root,
            };
            let display = display_for(&root);
            if let Some(existing) = reg.find_project_by_root(&root).map_err(ErrorBody::from)? {
                if existing.store_kind == StoreKind::Managed {
                    return Ok((
                        RegisterProjectResult {
                            project: existing,
                            created: false,
                        },
                        false,
                    ));
                }
                return Err(ErrorBody::new(
                    ErrorCode::StoreIdentityConflict,
                    format!(
                        "{} is already registered with store {}",
                        root.display(),
                        existing.store_path
                    ),
                ));
            }
            // A project-local SAGE store already exists: managed + local would
            // silently become two authoritative memories (architecture §3).
            let local = adopted_store_path(&root);
            if local.exists() {
                return Err(ErrorBody::new(
                    ErrorCode::StoreIdentityConflict,
                    format!(
                        "{} already has a SAGE store at {}; register it with store_kind=adopted instead",
                        root.display(),
                        local.display()
                    ),
                ));
            }
            let id = format!("p_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
            let store = data_root.projects_dir().join(&id).join("sage.db");
            SageStore::create_managed(&store, &now_iso())?;
            let info = reg
                .insert_project_with_id(
                    &id,
                    &NewProject {
                        display_name: &display,
                        root: &root,
                        store_path: &store,
                        store_kind: StoreKind::Managed,
                        want_write: true,
                        admission: Admission::ReadWrite,
                        admission_reason: "service-managed store",
                        storage_profile_id: Some(STORAGE_PROFILE_ID),
                    },
                )
                .map_err(ErrorBody::from)?;
            Ok((
                RegisterProjectResult {
                    project: info,
                    created: true,
                },
                false,
            ))
        }
    }
}
