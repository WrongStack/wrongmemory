//! Timing probe for a real-size store (ignored by default).
//!
//! Runs against a COPY given by `WM_PROBE_STORE` (for example one made with
//! `wrongmemory backup`), opened read-only. Never point it at a live store.
//!
//! ```powershell
//! $env:WM_PROBE_STORE = "<copy>\sage.db"
//! cargo test --release -p wrongmemory-storage --test store_probe -- --ignored --nocapture
//! ```

use std::time::Instant;

use wrongmemory_storage::{SageStore, Viewer};

#[test]
#[ignore = "needs WM_PROBE_STORE pointing at a store copy"]
fn time_reads_on_a_store_copy() {
    let path = std::env::var("WM_PROBE_STORE").expect("set WM_PROBE_STORE");
    let t = Instant::now();
    let (store, _) = SageStore::open(
        std::path::Path::new(&path),
        false,
        None,
        "2026-11-01T00:00:00.000Z",
    )
    .unwrap();
    println!("open: {:?}", t.elapsed());
    let viewer = Viewer::default();
    let t = Instant::now();
    let stats = store.stats(&viewer).unwrap();
    println!("stats: {:?} -> edges {}", t.elapsed(), stats["edges"]);
    let t = Instant::now();
    let page = store
        .list_page(None, None, None, Some(50), None, &viewer)
        .unwrap();
    println!("listPage: {:?} -> total {}", t.elapsed(), page.total);
    for q in ["build release check", "project server handshake"] {
        let t = Instant::now();
        let hits = store.search(q, 10, None, None, &viewer).unwrap().hits.len();
        println!("search {q:?}: {:?} -> {hits} hits", t.elapsed());
    }
    let t = Instant::now();
    let g = store.graph_for("scripts", 2, 100, &viewer).unwrap();
    println!(
        "graph_for: {:?} -> {} nodes",
        t.elapsed(),
        serde_json::to_value(&g).unwrap()["nodes"]
            .as_array()
            .map_or(0, Vec::len)
    );
}
