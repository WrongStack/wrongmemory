//! WM-007: multilingual and file-path corpus with positive controls and hard
//! negatives (`tests/fixtures/quality/multilingual-path.json`).
//!
//! Path queries are exact: `forPath` must return exactly the expected set.
//! Search queries must rank their target within the top five, except the
//! ones listed in `KNOWN_LEXICAL_MISSES`, which record where the baseline's
//! lexical tokenizer cannot match (and must keep failing, so the list cannot
//! rot). Hard negatives (another session's private note, superseded and
//! deleted records) must never be returned to the default viewer; look-alike
//! paths (`src/app.tsx`, `src/apps/`, `src/app.ts.bak`, `src/api-legacy`,
//! `lib/src/app.ts`) must never be returned for another path.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use serde_json::{Map, Value, json};
use wrongmemory_contracts::{MemoryAnchor, MemorySource, RememberParams};
use wrongmemory_storage::{MutationCtx, SageStore, Viewer};

/// Queries whose target the lexical search does not rank in the top five,
/// with the reason.
const KNOWN_LEXICAL_MISSES: &[(&str, &str)] = &[
    (
        "並列実行",
        "unspaced Japanese: the baseline term split (memory-text.ts tokenize) keeps a CJK \
         run as one term, so a substring cannot match; kept for search-order parity \
         (WM-500); a semantic provider (WM-508) is the route",
    ),
    (
        "日志保留",
        "unspaced Chinese: same inherited term split as above",
    ),
];

fn ctx(session: Option<&str>, now: &str) -> MutationCtx {
    MutationCtx {
        principal_id: "host".into(),
        client_label: "corpus".into(),
        session_id: session.map(str::to_string),
        idempotency_key: None,
        now_iso: now.into(),
    }
}

fn remember(store: &mut SageStore, m: &Value, now: &str) -> String {
    let session = m["session"].as_str();
    let mut p = RememberParams {
        text: m["text"].as_str().unwrap().into(),
        sources: vec![MemorySource {
            kind: "user".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    if session.is_some() {
        p.scope = Some("session".into());
    }
    for kind in ["file", "directory"] {
        if let Some(path) = m[kind].as_str() {
            p.anchors = vec![MemoryAnchor {
                kind: kind.into(),
                path: Some(path.into()),
                ..Default::default()
            }];
        }
    }
    store.remember(&p, &ctx(session, now)).unwrap().id
}

#[test]
fn multilingual_and_path_corpus_controls_hold() {
    let corpus: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/quality/multilingual-path.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let db = tmp.path().join("sage.db");
    SageStore::create_managed(&db, "2026-11-01T00:00:00.000Z").unwrap();
    let (mut store, _) =
        SageStore::open(&db, true, Some(root), "2026-11-01T00:00:00.000Z").unwrap();

    let mut second = 0;
    let mut now = || {
        second += 1;
        format!("2026-11-01T00:{:02}:{:02}.000Z", second / 60, second % 60)
    };
    let mut ids: HashMap<String, String> = HashMap::new();
    // Negatives first, so superseding targets are the newer records.
    for m in corpus["negatives"]
        .as_array()
        .unwrap()
        .iter()
        .chain(corpus["memories"].as_array().unwrap())
    {
        let id = remember(&mut store, m, &now());
        ids.insert(m["key"].as_str().unwrap().into(), id);
    }
    for m in corpus["negatives"].as_array().unwrap() {
        let mut patch = Map::new();
        if let Some(t) = m["supersededBy"].as_str() {
            patch.insert("status".into(), json!("superseded"));
            patch.insert("supersededBy".into(), json!(ids[t]));
        } else if m["delete"] == true {
            patch.insert("status".into(), json!("deleted"));
        } else {
            continue;
        }
        let id = &ids[m["key"].as_str().unwrap()];
        store.update(id, &patch, &ctx(None, &now()), true).unwrap();
    }
    let key_of: HashMap<&String, &str> = ids.iter().map(|(k, v)| (v, k.as_str())).collect();
    // Search must never surface a record the default viewer cannot see or
    // that is retired. Look-alike path records are active and visible: they
    // are hard negatives for path queries only (checked by the exact sets).
    let negatives: BTreeSet<&str> = corpus["negatives"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| {
            m["session"].is_string() || m["supersededBy"].is_string() || m["delete"] == true
        })
        .map(|m| m["key"].as_str().unwrap())
        .collect();
    let viewer = Viewer::default();
    let mut problems = Vec::new();

    for q in corpus["path"].as_array().unwrap() {
        let path = q["path"].as_str().unwrap();
        let expect: BTreeSet<&str> = q["expect"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();
        let got: BTreeSet<&str> = store
            .for_path(path, 50, &viewer)
            .unwrap()
            .hits
            .iter()
            .map(|h| key_of[&h.memory.id])
            .collect();
        if got != expect {
            problems.push(format!(
                "forPath {path:?}: expected {expect:?}, got {got:?}"
            ));
        }
        // Windows-style separators name the same project path.
        let backslashed = path.replace('/', "\\");
        let got_bs: BTreeSet<&str> = store
            .for_path(&backslashed, 50, &viewer)
            .unwrap()
            .hits
            .iter()
            .map(|h| key_of[&h.memory.id])
            .collect();
        if got_bs != expect {
            problems.push(format!(
                "forPath {backslashed:?}: expected {expect:?}, got {got_bs:?}"
            ));
        }
    }

    let mut ranked = 0;
    for q in corpus["search"].as_array().unwrap() {
        let text = q["q"].as_str().unwrap();
        let target = q["target"].as_str().unwrap();
        let hits = store.search(text, 10, None, None, &viewer).unwrap().hits;
        let keys: Vec<&str> = hits.iter().map(|h| key_of[&h.memory.id]).collect();
        for k in &keys {
            if negatives.contains(k) {
                problems.push(format!("search {text:?} returned hard negative {k}"));
            }
        }
        let rank = keys.iter().position(|k| *k == target);
        let known = KNOWN_LEXICAL_MISSES.iter().any(|(q, _)| *q == text);
        match (rank.is_some_and(|r| r < 5), known) {
            (true, false) => ranked += 1,
            (false, true) => {}
            (false, false) => problems.push(format!("search {text:?}: {target} at {rank:?}")),
            (true, true) => problems.push(format!(
                "search {text:?} now finds {target}; drop it from KNOWN_LEXICAL_MISSES"
            )),
        }
    }
    // WM-508/509: the same queries through the built-in n-gram provider and
    // daemon-side fusion. Every target, including the unspaced CJK ones the
    // lexical term split misses, must rank in the top five, and no hard
    // negative may enter through the vector channel.
    let provider = wrongmemory_storage::semantic::NgramHashProvider::new();
    let mut cache = wrongmemory_storage::semantic::SemanticCache::open(
        &tmp.path().join("cache").join("semantic.sqlite"),
        wrongmemory_storage::semantic::EmbeddingProvider::profile(&provider),
    )
    .unwrap();
    let mut hybrid_ranked = 0;
    for q in corpus["search"].as_array().unwrap() {
        let text = q["q"].as_str().unwrap();
        let target = q["target"].as_str().unwrap();
        let hits = store
            .hybrid_search(&mut cache, &provider, text, 10, None, None, &viewer)
            .unwrap()
            .hits;
        let keys: Vec<&str> = hits.iter().map(|h| key_of[&h.memory.id]).collect();
        for k in &keys {
            if negatives.contains(k) {
                problems.push(format!("hybrid {text:?} returned hard negative {k}"));
            }
        }
        match keys.iter().position(|k| *k == target) {
            Some(r) if r < 5 => hybrid_ranked += 1,
            rank => problems.push(format!("hybrid {text:?}: {target} at {rank:?} in {keys:?}")),
        }
    }
    println!("corpus hybrid: {hybrid_ranked} search targets in the top five");
    println!(
        "corpus: {ranked}/{} search targets in the top five, {} path queries",
        corpus["search"].as_array().unwrap().len(),
        corpus["path"].as_array().unwrap().len()
    );
    assert!(
        problems.is_empty(),
        "corpus controls:\n{}",
        problems.join("\n")
    );
}
