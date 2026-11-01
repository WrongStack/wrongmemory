//! WM-508/509: per-project semantic caches owned by the daemon.
//!
//! The provider choice lives in the registry (`semantic.provider:<id>`); the
//! derived vectors live in `<data_root>/cache/semantic/<projectId>.sqlite`,
//! outside every SAGE store. A cache is rebuildable: disabling the provider,
//! detaching or exporting the project deletes it. Lock order is always the
//! project's store lane first, then its cache.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wrongmemory_contracts::{ErrorBody, ErrorCode};
use wrongmemory_ipc::DataRoot;
use wrongmemory_storage::semantic::{EmbeddingProvider, SemanticCache, semantic_unavailable};

#[derive(Default)]
pub struct SemanticCaches {
    open: Mutex<HashMap<String, Arc<Mutex<SemanticCache>>>>,
}

pub fn cache_path(root: &DataRoot, project_id: &str) -> Result<PathBuf, ErrorBody> {
    // Project ids are daemon-minted; refuse anything that could leave the dir.
    if project_id.is_empty()
        || !project_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidRequest,
            "project id is not usable as a cache name",
        ));
    }
    Ok(root
        .path()
        .join("cache")
        .join("semantic")
        .join(format!("{project_id}.sqlite")))
}

impl SemanticCaches {
    /// The project's cache for `provider`'s profile, opened on first use.
    pub fn get(
        &self,
        root: &DataRoot,
        project_id: &str,
        provider: &dyn EmbeddingProvider,
    ) -> Result<Arc<Mutex<SemanticCache>>, ErrorBody> {
        let mut open = self.open.lock().map_err(|_| poisoned())?;
        if let Some(c) = open.get(project_id) {
            let same =
                c.lock().map_err(|_| poisoned())?.profile().key() == provider.profile().key();
            if same {
                return Ok(c.clone());
            }
        }
        let cache = Arc::new(Mutex::new(SemanticCache::open(
            &cache_path(root, project_id)?,
            provider.profile(),
        )?));
        open.insert(project_id.to_string(), cache.clone());
        Ok(cache)
    }

    /// Forget and delete the project's cache (it is derived data).
    pub fn drop_project(&self, root: &DataRoot, project_id: &str) {
        if let Ok(mut open) = self.open.lock() {
            open.remove(project_id);
        }
        if let Ok(path) = cache_path(root, project_id) {
            for suffix in ["", "-wal", "-shm", "-journal"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }
}

fn poisoned() -> ErrorBody {
    semantic_unavailable("semantic cache lock poisoned")
}

// ─── Request handlers (run on the blocking pool) ────────────────────────────

use serde_json::{Value, json};
use wrongmemory_contracts::{SearchParams, SearchResult};
use wrongmemory_storage::Viewer;
use wrongmemory_storage::semantic::{PROVIDER_NONE, PROVIDERS, provider_for};

use crate::server::Shared;

fn configured(
    s: &Shared,
    project_id: &str,
) -> Result<Option<Box<dyn EmbeddingProvider>>, ErrorBody> {
    Ok(s.reg(|r| r.semantic_provider(project_id))?
        .as_deref()
        .and_then(provider_for))
}

/// `memory.search` in any semantic mode. `lexical` never touches the cache.
/// `auto` falls back to lexical when no provider is configured or the
/// provider fails (automatic recall stays lexical); `required` reports it.
pub fn search(
    s: &Shared,
    project_id: &str,
    p: &SearchParams,
    mode: &str,
    viewer: &Viewer,
) -> Result<SearchResult, ErrorBody> {
    let h = s.stores.handle(&s.registry, &s.instance_id, project_id)?;
    let limit = p.limit.unwrap_or(20);
    let lexical = |store: &wrongmemory_storage::SageStore| {
        store.search(
            &p.query,
            limit,
            p.kinds.as_deref(),
            p.statuses.as_deref(),
            viewer,
        )
    };
    let provider = if mode == "lexical" {
        None
    } else {
        configured(s, project_id)?
    };
    let store = h.lock()?;
    let Some(provider) = provider else {
        if mode == "required" {
            return Err(semantic_unavailable(
                "no semantic provider is configured for this project (semantic.configure)",
            ));
        }
        return lexical(&store);
    };
    let fused = s
        .semantic
        .get(&s.root, project_id, provider.as_ref())
        .and_then(|cache| {
            let mut cache = cache.lock().map_err(|_| poisoned())?;
            store.hybrid_search(
                &mut cache,
                provider.as_ref(),
                &p.query,
                limit,
                p.kinds.as_deref(),
                p.statuses.as_deref(),
                viewer,
            )
        });
    match fused {
        Err(e) if mode == "auto" && e.code == ErrorCode::SemanticUnavailable => lexical(&store),
        other => other,
    }
}

pub fn status(s: &Shared, project_id: &str) -> Result<Value, ErrorBody> {
    let name = s
        .reg(|r| r.semantic_provider(project_id))?
        .unwrap_or_else(|| PROVIDER_NONE.into());
    let mut out = json!({ "provider": name, "providers": PROVIDERS });
    if let Some(provider) = provider_for(&name) {
        let cache = s.semantic.get(&s.root, project_id, provider.as_ref())?;
        let indexed = cache.lock().map_err(|_| poisoned())?.indexed()?;
        out["profile"] = serde_json::to_value(provider.profile()).unwrap_or(Value::Null);
        out["indexed"] = json!(indexed);
    }
    Ok(out)
}

/// Choose the provider. Enabling builds the cache first and persists the
/// choice only once that succeeded; `none` disables and deletes the cache.
pub fn configure(s: &Shared, project_id: &str, provider: &str) -> Result<Value, ErrorBody> {
    if !PROVIDERS.contains(&provider) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidRequest,
            format!(
                "unknown semantic provider {provider}; known: {}",
                PROVIDERS.join(", ")
            ),
        ));
    }
    let Some(p) = provider_for(provider) else {
        s.reg(|r| r.set_semantic_provider(project_id, None))?;
        s.semantic.drop_project(&s.root, project_id);
        return status(s, project_id);
    };
    let h = s.stores.handle(&s.registry, &s.instance_id, project_id)?;
    let store = h.lock()?;
    let cache = s.semantic.get(&s.root, project_id, p.as_ref())?;
    let report = {
        let mut cache = cache.lock().map_err(|_| poisoned())?;
        store.sync_semantic(&mut cache, p.as_ref())?
    };
    drop(store);
    s.reg(|r| r.set_semantic_provider(project_id, Some(provider)))?;
    let mut out = status(s, project_id)?;
    out["sync"] = serde_json::to_value(report).unwrap_or(Value::Null);
    Ok(out)
}
