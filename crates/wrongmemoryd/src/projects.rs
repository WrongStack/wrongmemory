//! Project registration, store admission, and per-project store handles.
//!
//! Each project has at most one open [`SageStore`] behind a mutex: the mutex
//! is the project's serialized writer lane, and SQLite work always runs on the
//! blocking pool. Writable handles hold the store's [`OwnerGuard`] and an
//! owner epoch recorded in the registry (WM-INV-01).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use wrongmemory_contracts::{
    Admission, ErrorBody, ErrorCode, ProjectInfo, RegisterProjectParams, RegisterProjectResult,
    STORAGE_PROFILE_ID, StoreKind,
};
use wrongmemory_ipc::DataRoot;
use wrongmemory_registry::{NewProject, Registry, now_iso};
use wrongmemory_storage::{OwnerGuard, SageStore, inspect};

pub struct ProjectHandle {
    pub info: ProjectInfo,
    pub store: Mutex<SageStore>,
    _guard: Option<OwnerGuard>,
}

#[derive(Default)]
pub struct Stores {
    open: Mutex<HashMap<String, Arc<ProjectHandle>>>,
}

fn internal(e: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, e.to_string())
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

/// SAGE's default project-local store location.
pub fn adopted_store_path(root: &Path) -> PathBuf {
    root.join(".wrongstack").join("memories").join("sage.db")
}

impl Stores {
    pub fn count(&self) -> usize {
        self.open.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Open (or reuse) the handle for a registered project. Blocking.
    pub fn handle(
        &self,
        registry: &Mutex<Registry>,
        instance_id: &str,
        project_id: &str,
    ) -> Result<Arc<ProjectHandle>, ErrorBody> {
        if let Some(h) = self.open.lock().map_err(internal)?.get(project_id) {
            return Ok(h.clone());
        }
        let info = registry
            .lock()
            .map_err(internal)?
            .get_project(project_id)
            .map_err(ErrorBody::from)?
            .ok_or_else(|| ErrorBody::new(ErrorCode::ProjectNotFound, "project not found"))?;
        let handle = Arc::new(open_handle(registry, instance_id, info)?);
        let mut open = self.open.lock().map_err(internal)?;
        // Another request may have opened it meanwhile; keep the first.
        Ok(open.entry(project_id.to_string()).or_insert(handle).clone())
    }

    pub fn close(&self, project_id: &str) -> Option<Arc<ProjectHandle>> {
        self.open.lock().ok()?.remove(project_id)
    }

    pub fn drain_all(&self) -> Vec<Arc<ProjectHandle>> {
        self.open
            .lock()
            .map(|mut m| m.drain().map(|(_, v)| v).collect())
            .unwrap_or_default()
    }
}

fn open_handle(
    registry: &Mutex<Registry>,
    instance_id: &str,
    mut info: ProjectInfo,
) -> Result<ProjectHandle, ErrorBody> {
    let store_path = PathBuf::from(&info.store_path);
    let root = PathBuf::from(&info.root);
    match info.admission {
        Admission::Refused => Err(ErrorBody::new(
            ErrorCode::ProfileUnsupported,
            format!("store admission refused: {}", info.admission_reason),
        )),
        Admission::ReadOnly => {
            let (store, _) = SageStore::open(&store_path, false, Some(root), &now_iso())?;
            Ok(ProjectHandle {
                info,
                store: Mutex::new(store),
                _guard: None,
            })
        }
        Admission::ReadWrite => {
            let epoch_next = info.owner_epoch + 1;
            let guard = OwnerGuard::acquire(&store_path, epoch_next, instance_id).map_err(|e| {
                ErrorBody::new(
                    ErrorCode::OwnerConflict,
                    format!("cannot acquire store ownership: {e}"),
                )
            })?;
            let (store, _) = SageStore::open(&store_path, true, Some(root), &now_iso())?;
            let epoch = registry
                .lock()
                .map_err(internal)?
                .begin_ownership(&info.project_id, instance_id)
                .map_err(ErrorBody::from)?;
            info.owner_epoch = epoch;
            Ok(ProjectHandle {
                info,
                store: Mutex::new(store),
                _guard: Some(guard),
            })
        }
    }
}

/// `project.register`. Blocking.
pub fn register(
    data_root: &DataRoot,
    registry: &Mutex<Registry>,
    p: &RegisterProjectParams,
) -> Result<RegisterProjectResult, ErrorBody> {
    let root = canonical_dir(Path::new(&p.root))?;
    let display = p.display_name.clone().unwrap_or_else(|| {
        root.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string())
    });
    let mut reg = registry.lock().map_err(internal)?;
    match p.store_kind {
        StoreKind::Adopted => {
            let store = adopted_store_path(&root);
            if let Some(existing) = reg.find_project_by_store(&store).map_err(ErrorBody::from)? {
                return Ok(RegisterProjectResult {
                    project: existing,
                    created: false,
                });
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
            } else if insp.legacy_owner_metadata && !p.force_owner {
                (
                    Admission::ReadOnly,
                    "legacy SAGE project-server metadata (server.json) present; an unmodified \
                     SAGE owner ignores WrongMemory's owner guard. Stop it and re-register with \
                     force_owner to acknowledge."
                        .to_string(),
                )
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
            Ok(RegisterProjectResult {
                project: info,
                created: true,
            })
        }
        StoreKind::Managed => {
            if let Some(existing) = reg.find_project_by_root(&root).map_err(ErrorBody::from)? {
                if existing.store_kind == StoreKind::Managed {
                    return Ok(RegisterProjectResult {
                        project: existing,
                        created: false,
                    });
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
            Ok(RegisterProjectResult {
                project: info,
                created: true,
            })
        }
    }
}
