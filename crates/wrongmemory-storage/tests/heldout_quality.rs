//! WM-605 held-out evaluation on real tasks
//! (`tests/fixtures/quality/heldout-wrongstack.json`).
//!
//! The corpus was written by an agent that had no access to this crate's
//! retrieval code. Its tasks are real commits from the public WrongStack
//! repository, and its ground truth lists the project memories each task
//! needs. This harness was written before the corpus existed, and the
//! ranking was not changed after the measurement.
//!
//! The test measures the product's retrieval channels separately:
//! - `lexical`: `memory.search` on the task text;
//! - `hybrid`: lexical fused with the built-in n-gram semantic provider;
//! - `path`: `memory.forPath` on the files the commit touched;
//! - `recipe`: what the agent guidance asks for, which is the search top 5
//!   followed by the path top 5 (at most 10 records).
//!
//! It reports required-fact recall, precision within the budget and MRR.
//! It asserts only isolation: another session's notes, superseded records
//! and contradicted records must never be returned. There is no quality
//! floor, because a floor set from these cases would stop them being held
//! out. Set `WM_HELDOUT_OUT=<file>` to write the report as JSON.
//!
//! Set `WM_HELDOUT_BACKGROUND=<sage.db>` to add the corpus to a copy of a
//! large store instead of an empty one, to see how ranking holds up at scale.
//! Pass a consistent backup, never a live store: the harness writes to its
//! copy only. Background records are not in the ground truth, so one that
//! answers a task as well as the corpus memory pushes it down and counts
//! against recall; the result is a lower bound, and precision is not
//! meaningful in this mode. `WM_HELDOUT_DUMP=<file>` writes each task's
//! lexical and path rankings with record excerpts, for reading what outranks
//! the corpus. With a background store it holds that store's text: keep the
//! file local. `WM_HELDOUT_AS_OF=<file>` maps each task's commit to its UTC
//! commit time; background records created after it are dropped from that
//! task's rankings, so a task sees roughly the store as it was then (records
//! deleted or retired since are not restored).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{Map, Value, json};
use wrongmemory_contracts::{MemoryAnchor, MemorySource, RememberParams, SearchHit};
use wrongmemory_storage::retrieval::PathOptions;
use wrongmemory_storage::semantic::{EmbeddingProvider, NgramHashProvider, SemanticCache};
use wrongmemory_storage::{MutationCtx, SageStore, Viewer};

const BUDGET: usize = 10;

fn ctx(session: Option<&str>, now: &str) -> MutationCtx {
    MutationCtx {
        principal_id: "host".into(),
        client_label: "heldout".into(),
        session_id: session.map(str::to_string),
        idempotency_key: None,
        now_iso: now.into(),
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn remember(store: &mut SageStore, m: &Value, now: &str) -> String {
    let session = m["session"].as_str();
    let mut p = RememberParams {
        text: m["text"].as_str().unwrap().into(),
        kind: m["kind"].as_str().map(str::to_string),
        tags: strings(&m["tags"]),
        sources: vec![MemorySource {
            kind: "user".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    if session.is_some() {
        p.scope = Some("session".into());
    }
    p.anchors = strings(&m["files"])
        .into_iter()
        .map(|f| MemoryAnchor {
            kind: "file".into(),
            path: Some(f),
            ..Default::default()
        })
        .collect();
    store
        .remember(&p, &ctx(session, now))
        .unwrap_or_else(|e| panic!("remember {}: {e:?}", m["key"]))
        .id
}

#[derive(Default)]
struct Channel {
    recall5: f64,
    recall10: f64,
    all_found: usize,
    rr: f64,
    precision5: f64,
    empty: usize,
    misses: Vec<Value>,
}

impl Channel {
    fn score(&mut self, task: &Value, ranked: &[String], required: &HashSet<String>) {
        let found = |k: usize| {
            ranked
                .iter()
                .take(k)
                .filter(|id| required.contains(*id))
                .count()
        };
        let need = required.len() as f64;
        self.recall5 += found(5) as f64 / need;
        self.recall10 += found(BUDGET) as f64 / need;
        self.all_found += usize::from(found(BUDGET) == required.len());
        if let Some(r) = ranked.iter().position(|id| required.contains(id)) {
            self.rr += 1.0 / (r as f64 + 1.0);
        }
        let top5 = ranked.len().min(5);
        if top5 > 0 {
            self.precision5 += found(5) as f64 / top5 as f64;
        }
        self.empty += usize::from(ranked.is_empty());
        if found(BUDGET) < required.len() {
            self.misses.push(json!({
                "commit": task["commit"],
                "found": found(BUDGET),
                "required": required.len(),
                "returned": ranked.len(),
            }));
        }
    }

    fn report(&self, n: usize) -> Value {
        let n = n as f64;
        let r = |x: f64| (x / n * 1000.0).round() / 1000.0;
        json!({
            "recallAt5": r(self.recall5),
            "recallAt10": r(self.recall10),
            "allRequiredAt10": r(self.all_found as f64),
            "mrr": r(self.rr),
            "precisionAt5": r(self.precision5),
            "emptyResults": self.empty,
            "tasksWithMisses": self.misses.len(),
            "misses": self.misses,
        })
    }
}

fn ids(hits: &[SearchHit]) -> Vec<String> {
    hits.iter().map(|h| h.memory.id.clone()).collect()
}

/// One ranking for `WM_HELDOUT_DUMP`.
fn dump_hits(
    hits: &[SearchHit],
    key_of: &HashMap<&String, &String>,
    required: &HashSet<String>,
) -> Value {
    hits.iter()
        .map(|h| {
            let m = &h.memory;
            json!({
                "id": m.id,
                "corpusKey": key_of.get(&m.id),
                "required": required.contains(&m.id),
                "kind": m.kind,
                "status": m.status,
                "createdAt": m.created_at,
                "sources": m.sources.iter().map(|s| s.kind.clone()).collect::<Vec<_>>(),
                "anchors": m.anchors.iter().filter_map(|a| a.path.clone()).collect::<Vec<_>>(),
                "text": m.text.chars().take(240).collect::<String>(),
            })
        })
        .collect()
}

#[test]
fn heldout_real_task_recall_is_measured_without_leaks() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/quality/heldout-wrongstack.json");
    let corpus: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("sage.db");
    let background = std::env::var_os("WM_HELDOUT_BACKGROUND");
    match &background {
        Some(src) => {
            std::fs::copy(src, &db).unwrap();
        }
        None => SageStore::create_managed(&db, "2026-11-03T00:00:00.000Z").unwrap(),
    }
    let (mut store, _) = SageStore::open(&db, true, None, "2026-11-03T00:00:00.000Z").unwrap();
    let background_stats = background
        .is_some()
        .then(|| store.stats(&Viewer::default()).unwrap()["byStatus"].clone());

    let mut second = 0;
    let mut now = || {
        second += 1;
        format!(
            "2026-11-03T{:02}:{:02}:{:02}.000Z",
            second / 3600,
            second / 60 % 60,
            second % 60
        )
    };
    // Negatives first, so the records they are outdated by are newer.
    let mut by_key: HashMap<String, String> = HashMap::new();
    let mut merged = Vec::new();
    for group in ["negatives", "memories"] {
        for m in corpus[group].as_array().unwrap() {
            let key = m["key"].as_str().unwrap().to_string();
            let id = remember(&mut store, m, &now());
            if let Some((other, _)) = by_key.iter().find(|(_, v)| **v == id) {
                merged.push(format!("{key} merged into {other}"));
            }
            assert!(
                by_key.insert(key.clone(), id).is_none(),
                "duplicate key {key}"
            );
        }
    }
    for m in corpus["negatives"].as_array().unwrap() {
        let id = &by_key[m["key"].as_str().unwrap()];
        let mut patch = Map::new();
        if let Some(t) = m["supersededBy"].as_str() {
            patch.insert("status".into(), json!("superseded"));
            patch.insert("supersededBy".into(), json!(by_key[t]));
        } else if m["contradicts"].is_string() {
            patch.insert("status".into(), json!("contradicted"));
        } else {
            continue;
        }
        store.update(id, &patch, &ctx(None, &now()), false).unwrap();
    }
    let negatives: HashSet<&String> = corpus["negatives"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &by_key[m["key"].as_str().unwrap()])
        .collect();

    let provider = NgramHashProvider::new();
    let mut cache =
        SemanticCache::open(&tmp.path().join("semantic.sqlite"), provider.profile()).unwrap();
    let viewer = Viewer::default();
    let tasks = corpus["tasks"].as_array().unwrap();
    let mut channels: Vec<(&str, Channel)> = ["lexical", "hybrid", "path", "recipe"]
        .into_iter()
        .map(|c| (c, Channel::default()))
        .collect();
    let mut leaks = Vec::new();
    let key_of: HashMap<&String, &String> = by_key.iter().map(|(k, v)| (v, k)).collect();
    let mut dump = Vec::new();
    let as_of: Option<Value> = std::env::var("WM_HELDOUT_AS_OF")
        .ok()
        .map(|f| serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap());
    // With a cutoff, fetch more so that dropping later records still leaves
    // a full budget.
    let fetch = if as_of.is_some() {
        wrongmemory_contracts::MAX_PAGE_SIZE
    } else {
        BUDGET
    };
    for task in tasks {
        let text = task["task"].as_str().unwrap();
        let required: HashSet<String> = strings(&task["required"])
            .iter()
            .map(|k| by_key[k].clone())
            .collect();
        // As of the commit: drop background records written after it.
        let cutoff = as_of
            .as_ref()
            .map(|m| m[task["commit"].as_str().unwrap()].as_str().unwrap());
        let known = |hits: Vec<SearchHit>| -> Vec<SearchHit> {
            hits.into_iter()
                .filter(|h| {
                    key_of.contains_key(&h.memory.id)
                        || cutoff.is_none_or(|c| h.memory.created_at.as_str() <= c)
                })
                .take(BUDGET)
                .collect()
        };
        let lexical_hits = known(store.search(text, fetch, None, None, &viewer).unwrap().hits);
        let lexical = ids(&lexical_hits);
        let hybrid = ids(&known(
            store
                .hybrid_search(&mut cache, &provider, text, fetch, None, None, &viewer)
                .unwrap()
                .hits,
        ));
        let path_hits = known(
            store
                .retrieve_for_paths(
                    &strings(&task["paths"]),
                    &PathOptions {
                        limit: Some(fetch),
                        ..Default::default()
                    },
                    &viewer,
                )
                .unwrap()
                .hits,
        );
        let path = ids(&path_hits);
        dump.push(json!({
            "commit": task["commit"],
            "task": text.chars().take(300).collect::<String>(),
            "required": task["required"],
            "lexical": dump_hits(&lexical_hits, &key_of, &required),
            "path": dump_hits(&path_hits, &key_of, &required),
        }));
        let mut recipe: Vec<String> = lexical.iter().take(5).cloned().collect();
        for id in path.iter().take(5) {
            if !recipe.contains(id) {
                recipe.push(id.clone());
            }
        }
        for ((name, channel), ranked) in
            channels.iter_mut().zip([&lexical, &hybrid, &path, &recipe])
        {
            channel.score(task, ranked, &required);
            for id in ranked.iter().filter(|id| negatives.contains(id)) {
                leaks.push(format!("{name} {}: returned negative {id}", task["commit"]));
            }
        }
    }
    let report = json!({
        "schema": "wrongmemory/heldout-quality@1",
        "corpus": {
            "source": corpus["source"],
            "memories": corpus["memories"].as_array().unwrap().len(),
            "negatives": negatives.len(),
            "tasks": tasks.len(),
            "requiredLinks": tasks.iter().map(|t| strings(&t["required"]).len()).sum::<usize>(),
            "mergedOnWrite": merged,
        },
        "background": background_stats.map(|s| json!({"recordsByStatus": s, "asOfCommit": as_of.is_some()})),
        "budget": BUDGET,
        "channels": channels.iter().map(|(n, c)| (n.to_string(), c.report(tasks.len()))).collect::<Map<_, _>>(),
        "leaks": leaks,
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if let Ok(out) = std::env::var("WM_HELDOUT_DUMP") {
        std::fs::write(out, serde_json::to_string_pretty(&dump).unwrap()).unwrap();
    }
    if let Ok(out) = std::env::var("WM_HELDOUT_OUT") {
        std::fs::write(out, serde_json::to_string_pretty(&report).unwrap() + "\n").unwrap();
    }
    assert!(leaks.is_empty(), "hard negatives returned: {leaks:#?}");
}
