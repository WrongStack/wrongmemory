//! WM-605: retrieval-quality regression budget on a hand-written project
//! memory corpus (`tests/fixtures/quality/corpus.json`).
//!
//! Every query is a natural-language paraphrase with exactly one applicable
//! memory. Hard negatives (superseded predecessors, a contradicted fact,
//! another session's private notes) must never be returned. The measured
//! recall@1, recall@5 and MRR must stay at or above the recorded budget; the
//! test prints the current numbers so a deliberate improvement can raise it.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Value, json};
use wrongmemory_contracts::{MemoryAnchor, MemorySource, RememberParams};
use wrongmemory_storage::{MutationCtx, SageStore, Viewer};

/// Measured 2026-11-01 (lexical search, no semantic provider): recall@1
/// 0.975, recall@5 0.975, MRR 0.979 over 40 queries, 0 hard negatives.
/// The budget allows one more query to slip. Lower it only with a
/// documented reason in docs/status.md.
const BUDGET_RECALL_AT_1: f64 = 0.95;
const BUDGET_RECALL_AT_5: f64 = 0.95;
const BUDGET_MRR: f64 = 0.95;

fn ctx(session: Option<&str>, now: &str) -> MutationCtx {
    MutationCtx {
        principal_id: "host".into(),
        client_label: "quality".into(),
        session_id: session.map(str::to_string),
        idempotency_key: None,
        now_iso: now.into(),
    }
}

fn remember(store: &mut SageStore, m: &Value, now: &str) -> String {
    let session = m["session"].as_str();
    let mut p = RememberParams {
        text: m["text"].as_str().unwrap().into(),
        kind: m["kind"].as_str().map(str::to_string),
        tags: m["tags"]
            .as_array()
            .map(|a| a.iter().map(|t| t.as_str().unwrap().to_string()).collect())
            .unwrap_or_default(),
        sources: vec![MemorySource {
            kind: "user".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    if session.is_some() {
        p.scope = Some("session".into());
    }
    if let Some(f) = m["file"].as_str() {
        p.anchors = vec![MemoryAnchor {
            kind: "file".into(),
            path: Some(f.into()),
            ..Default::default()
        }];
    }
    store.remember(&p, &ctx(session, now)).unwrap().id
}

#[test]
fn retrieval_quality_stays_within_budget() {
    let corpus: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/quality/corpus.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("sage.db");
    SageStore::create_managed(&db, "2026-11-01T00:00:00.000Z").unwrap();
    let (mut store, _) = SageStore::open(&db, true, None, "2026-11-01T00:00:00.000Z").unwrap();

    let mut ids: HashMap<String, String> = HashMap::new();
    let mut second = 0;
    let mut now = || {
        second += 1;
        format!("2026-11-01T00:{:02}:{:02}.000Z", second / 60, second % 60)
    };
    // Negatives first so their targets are the newer records.
    for m in corpus["negatives"].as_array().unwrap() {
        ids.insert(
            m["key"].as_str().unwrap().into(),
            remember(&mut store, m, &now()),
        );
    }
    for m in corpus["memories"].as_array().unwrap() {
        ids.insert(
            m["key"].as_str().unwrap().into(),
            remember(&mut store, m, &now()),
        );
    }
    for m in corpus["negatives"].as_array().unwrap() {
        let id = &ids[m["key"].as_str().unwrap()];
        let mut patch = Map::new();
        if let Some(t) = m["supersededBy"].as_str() {
            patch.insert("status".into(), json!("superseded"));
            patch.insert("supersededBy".into(), json!(ids[t]));
        } else if m["contradicts"].is_string() {
            patch.insert("status".into(), json!("contradicted"));
        } else {
            continue;
        }
        store.update(id, &patch, &ctx(None, &now()), false).unwrap();
    }
    let negatives: Vec<&String> = corpus["negatives"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &ids[m["key"].as_str().unwrap()])
        .collect();

    let viewer = Viewer::default();
    let queries = corpus["queries"].as_array().unwrap();
    let (mut at1, mut at5, mut rr) = (0usize, 0usize, 0.0f64);
    let mut misses = Vec::new();
    let mut leaks = Vec::new();
    for q in queries {
        let text = q["q"].as_str().unwrap();
        let target = &ids[q["target"].as_str().unwrap()];
        let hits = store.search(text, 10, None, None, &viewer).unwrap().hits;
        let rank = hits.iter().position(|h| &h.memory.id == target);
        if let Some(r) = rank {
            rr += 1.0 / (r as f64 + 1.0);
            at1 += usize::from(r == 0);
            at5 += usize::from(r < 5);
        }
        if rank.is_none_or(|r| r >= 5) {
            misses.push(format!("{text:?} -> rank {rank:?}"));
        }
        for h in &hits {
            if negatives.contains(&&h.memory.id) {
                leaks.push(format!(
                    "{text:?} returned a hard negative {}",
                    h.memory.text
                ));
            }
        }
    }
    // WM-508/509 measurement: the same queries through hybrid search.
    let provider = wrongmemory_storage::semantic::NgramHashProvider::new();
    let mut cache = wrongmemory_storage::semantic::SemanticCache::open(
        &tmp.path().join("semantic.sqlite"),
        wrongmemory_storage::semantic::EmbeddingProvider::profile(&provider),
    )
    .unwrap();
    let (mut h1, mut h5, mut hrr) = (0usize, 0usize, 0.0f64);
    let mut hybrid_misses = Vec::new();
    for q in queries {
        let text = q["q"].as_str().unwrap();
        let target = &ids[q["target"].as_str().unwrap()];
        let hits = store
            .hybrid_search(&mut cache, &provider, text, 10, None, None, &viewer)
            .unwrap()
            .hits;
        let rank = hits.iter().position(|h| &h.memory.id == target);
        if let Some(r) = rank {
            hrr += 1.0 / (r as f64 + 1.0);
            h1 += usize::from(r == 0);
            h5 += usize::from(r < 5);
        }
        if rank.is_none_or(|r| r >= 5) {
            hybrid_misses.push(format!("{text:?} -> rank {rank:?}"));
        }
        for h in &hits {
            if negatives.contains(&&h.memory.id) {
                leaks.push(format!(
                    "hybrid {text:?} returned a hard negative {}",
                    h.memory.text
                ));
            }
        }
    }
    let n = queries.len() as f64;
    let (hr1, hr5, hmrr) = (h1 as f64 / n, h5 as f64 / n, hrr / n);
    println!(
        "quality hybrid: recall@1={hr1:.3} recall@5={hr5:.3} mrr={hmrr:.3} misses={hybrid_misses:#?}"
    );
    let (r1, r5, mrr) = (at1 as f64 / n, at5 as f64 / n, rr / n);
    println!(
        "quality: queries={} recall@1={r1:.3} recall@5={r5:.3} mrr={mrr:.3} misses={misses:#?}",
        queries.len()
    );
    assert!(leaks.is_empty(), "hard negatives returned: {leaks:#?}");
    assert!(
        r1 >= BUDGET_RECALL_AT_1,
        "recall@1 {r1:.3} < {BUDGET_RECALL_AT_1}"
    );
    assert!(
        r5 >= BUDGET_RECALL_AT_5,
        "recall@5 {r5:.3} < {BUDGET_RECALL_AT_5}"
    );
    assert!(mrr >= BUDGET_MRR, "mrr {mrr:.3} < {BUDGET_MRR}");
    // Hybrid (n-gram provider, daemon fusion) measured 2026-11-01: recall@1
    // 0.975, recall@5 1.000, MRR 0.988. It must never fall below lexical.
    assert!(hr1 >= r1 && hr5 >= r5, "hybrid recall below lexical");
    assert!(
        hr1 >= BUDGET_RECALL_AT_1 && hr5 >= BUDGET_RECALL_AT_5 && hmrr >= BUDGET_MRR,
        "hybrid below budget: {hr1:.3} {hr5:.3} {hmrr:.3}"
    );
}
