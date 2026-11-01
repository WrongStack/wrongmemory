//! WM-509: the vector channel never widens visibility. The semantic cache
//! holds every active row (all sessions); fusion must re-apply the lexical
//! search's filter when it materializes a vector-only hit.

use serde_json::{Map, json};
use wrongmemory_contracts::{MemorySource, RememberParams};
use wrongmemory_storage::semantic::{EmbeddingProvider, NgramHashProvider, SemanticCache};
use wrongmemory_storage::{MutationCtx, SageStore, Viewer};

fn ctx(session: Option<&str>) -> MutationCtx {
    MutationCtx {
        principal_id: "host".into(),
        client_label: "test".into(),
        session_id: session.map(str::to_string),
        idempotency_key: None,
        now_iso: "2026-11-01T00:00:00.000Z".into(),
    }
}

fn remember(store: &mut SageStore, text: &str, session: Option<&str>) -> String {
    let p = RememberParams {
        text: text.into(),
        scope: session.map(|_| "session".to_string()),
        sources: vec![MemorySource {
            kind: "user".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    store.remember(&p, &ctx(session)).unwrap().id
}

fn ids(
    store: &SageStore,
    provider: &NgramHashProvider,
    cache: &mut SemanticCache,
    viewer: &Viewer,
) -> Vec<String> {
    store
        .hybrid_search(cache, provider, "キャッシュ 再構築", 10, None, None, viewer)
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.memory.id)
        .collect()
}

#[test]
fn vector_channel_respects_sessions_policy_and_status() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("sage.db");
    SageStore::create_managed(&db, "2026-11-01T00:00:00.000Z").unwrap();
    let (mut store, _) = SageStore::open(&db, true, None, "2026-11-01T00:00:00.000Z").unwrap();
    // Unspaced CJK: only the vector channel can match a substring query.
    let shared = remember(&mut store, "共有キャッシュは毎晩再構築されます。", None);
    let private = remember(
        &mut store,
        "非公開キャッシュは毎晩再構築されます。",
        Some("s1"),
    );
    let never = remember(&mut store, "禁止キャッシュは毎晩再構築されます。", None);
    let archived = remember(&mut store, "旧式キャッシュは毎晩再構築されます。", None);
    // Planted in the disposable fixture: `never` is set by lifecycle paths,
    // not by ordinary update.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE memories SET data = json_set(data, '$.contextPolicy', 'never') WHERE id = ?1",
            [&never],
        )
        .unwrap();
    let mut patch = Map::new();
    patch.insert("status".into(), json!("archived"));
    store.update(&archived, &patch, &ctx(None), false).unwrap();

    let provider = NgramHashProvider::new();
    let mut cache =
        SemanticCache::open(&tmp.path().join("semantic.sqlite"), provider.profile()).unwrap();
    // The lexical channel alone finds nothing for this substring query.
    assert!(
        store
            .search("キャッシュ 再構築", 10, None, None, &Viewer::default())
            .unwrap()
            .hits
            .is_empty()
    );
    let anyone = ids(&store, &provider, &mut cache, &Viewer::default());
    assert_eq!(
        anyone,
        vec![shared.clone()],
        "only the shared, active, injectable row"
    );
    let owner = ids(
        &store,
        &provider,
        &mut cache,
        &Viewer {
            session_id: Some("s1".into()),
        },
    );
    assert!(owner.contains(&shared) && owner.contains(&private));
    assert!(!owner.contains(&never) && !owner.contains(&archived));
    let other = ids(
        &store,
        &provider,
        &mut cache,
        &Viewer {
            session_id: Some("s2".into()),
        },
    );
    assert_eq!(other, vec![shared.clone()]);

    // An edit re-embeds (fingerprint moved): the old text no longer matches.
    let mut patch = Map::new();
    patch.insert(
        "text".into(),
        json!("Shared build cache is rebuilt nightly."),
    );
    store.update(&shared, &patch, &ctx(None), false).unwrap();
    assert!(ids(&store, &provider, &mut cache, &Viewer::default()).is_empty());
}

/// Hidden rows must not take vector slots: twenty other-session rows closer
/// to the query than the one visible row must not crowd it out of the
/// materialization budget (12) or change what the caller gets.
#[test]
fn hidden_rows_never_take_vector_slots() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("sage.db");
    SageStore::create_managed(&db, "2026-11-01T00:00:00.000Z").unwrap();
    let (mut store, _) = SageStore::open(&db, true, None, "2026-11-01T00:00:00.000Z").unwrap();
    let provider = NgramHashProvider::new();
    let mut cache =
        SemanticCache::open(&tmp.path().join("semantic.sqlite"), provider.profile()).unwrap();
    let visible = remember(
        &mut store,
        "共有キャッシュの再構築手順は運用文書にあります。",
        None,
    );
    let query = "キャッシュ 再構築";
    let before = ids(&store, &provider, &mut cache, &Viewer::default());
    assert_eq!(before, vec![visible.clone()]);
    for i in 0..20 {
        remember(
            &mut store,
            &format!("キャッシュ再構築{i}"),
            Some("other-session"),
        );
    }
    let after = store
        .hybrid_search(
            &mut cache,
            &provider,
            query,
            10,
            None,
            None,
            &Viewer::default(),
        )
        .unwrap();
    assert_eq!(
        after
            .hits
            .iter()
            .map(|h| h.memory.id.clone())
            .collect::<Vec<_>>(),
        before,
        "hidden rows changed the visible result"
    );
}
