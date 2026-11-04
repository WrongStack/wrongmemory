//! Timing probe for a hygiene pass on a real-size store (ignored by default).
//!
//! Copies the store given by `WM_PROBE_STORE` (for example one made with
//! `wrongmemory backup`) into a temporary directory and runs two passes on
//! the copy, with the options WrongStack schedules. `WM_PROBE_ROOT`, when
//! set, is the project root for anchor verification. Never point it at a
//! live store: the copy is written, the original is only read.
//!
//! Each pass prints its time, its report, and a digest of the copy's
//! memories, edges and candidates (without generated candidate ids), so two
//! builds can be compared on the same input.
//!
//! ```powershell
//! $env:WM_PROBE_STORE = "<copy>\sage.db"
//! cargo test --release -p wrongmemory-storage --test hygiene_probe -- --ignored --nocapture
//! ```

use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Instant;

use wrongmemory_storage::hygiene::HygieneOptions;
use wrongmemory_storage::{MutationCtx, SageStore};

#[test]
#[ignore = "needs WM_PROBE_STORE pointing at a store copy"]
fn time_hygiene_on_a_store_copy() {
    let src = std::env::var("WM_PROBE_STORE").expect("set WM_PROBE_STORE");
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sage.db");
    std::fs::copy(&src, &path).unwrap();
    let root = std::env::var("WM_PROBE_ROOT").ok().map(Into::into);
    let now = "2026-11-04T18:00:00.000Z";
    let (mut store, _) = SageStore::open(&path, true, root, now).unwrap();
    let opts: HygieneOptions =
        serde_json::from_str(r#"{"retentionDays":90,"archiveLowConfidenceAfterDays":30}"#).unwrap();
    let ctx = MutationCtx {
        principal_id: "daemon:maintenance".into(),
        client_label: "probe".into(),
        session_id: None,
        idempotency_key: None,
        now_iso: now.into(),
    };
    for pass in 0..2 {
        let t = Instant::now();
        let report = store.hygiene(&opts, &ctx).unwrap();
        let took = t.elapsed();
        let conn = rusqlite::Connection::open(&path).unwrap();
        let mut h = DefaultHasher::new();
        for sql in [
            "SELECT id || '|' || status || '|' || data FROM memories ORDER BY id",
            "SELECT from_node || '|' || to_node || '|' || relation FROM edges ORDER BY 1",
            "SELECT json_extract(data, '$.memoryId') || '|' || json_extract(data, '$.reviewReason') \
             || '|' || status || '|' || json_extract(data, '$.text') FROM candidates ORDER BY 1",
        ] {
            let mut s = conn.prepare(sql).unwrap();
            for row in s.query_map([], |r| r.get::<_, Option<String>>(0)).unwrap() {
                row.unwrap().hash(&mut h);
            }
        }
        eprintln!(
            "pass {pass}: {took:?}, digest {:016x}, report {}",
            h.finish(),
            serde_json::to_string(&report).unwrap()
        );
    }
}
