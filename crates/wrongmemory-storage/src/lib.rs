//! SAGE-compatible SQLite storage for WrongMemory.
//!
//! * [`inspect`] — read-only profile admission; never mutates a store.
//! * [`schema`] — SAGE v5 DDL (managed stores) and the additive extension.
//! * [`store`] — visibility-bound reads and the M1 write slice.
//! * [`owner`] — OS-backed owner guard per store.
//! * [`text`] — JS-compatible text/path normalization.

pub mod backup;
pub mod candidates;
pub mod doctor;
pub mod find_file;
pub mod hq_sync;
pub mod hygiene;
pub mod inspect;
pub mod legacy;
pub mod mutate;
pub mod owner;
pub mod paths;
pub mod recovery;
pub mod retrieval;
pub mod rules;
pub mod schema;
pub mod similarity;
pub mod store;
pub mod text;
pub mod unified_search;
pub mod verify;
pub mod wrongstack_paths;

pub use inspect::{Inspection, Verdict, inspect};
pub use owner::OwnerGuard;
pub use store::{MutationCtx, SageStore, Viewer};

/// Bundled SQLite version (recorded in health/doctor output).
pub fn sqlite_version() -> &'static str {
    rusqlite::version()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;
    use wrongmemory_contracts::{
        ErrorCode, MemoryAnchor, ProposeParams, RememberParams, SAGE_SQLITE_SCHEMA_VERSION,
    };

    const NOW: &str = "2026-11-01T12:00:00.000Z";

    fn ctx(session: Option<&str>, key: Option<&str>) -> MutationCtx {
        MutationCtx {
            principal_id: "host".into(),
            client_label: "test".into(),
            session_id: session.map(str::to_string),
            idempotency_key: key.map(str::to_string),
            now_iso: NOW.into(),
        }
    }

    fn managed() -> (tempfile::TempDir, SageStore) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p").join("sage.db");
        SageStore::create_managed(&path, NOW).unwrap();
        let (store, insp) = SageStore::open(&path, true, Some(tmp.path().into()), NOW).unwrap();
        assert_eq!(insp.verdict, Verdict::Compatible, "{}", insp.detail);
        (tmp, store)
    }

    fn remember(store: &mut SageStore, text: &str, session: Option<&str>) -> String {
        let p = RememberParams {
            text: text.into(),
            scope: session.map(|_| "session".to_string()),
            anchors: vec![MemoryAnchor {
                kind: "file".into(),
                path: Some("src/app/main.rs".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        store.remember(&p, &ctx(session, None)).unwrap().id
    }

    #[test]
    fn managed_store_is_a_compatible_v5_sage_store() {
        let (_t, store) = managed();
        let insp = store.inspect().unwrap();
        assert_eq!(insp.verdict, Verdict::Compatible);
        assert_eq!(insp.schema_meta["version"], SAGE_SQLITE_SCHEMA_VERSION);
        assert_eq!(insp.schema_meta["fts_index_initialized"], 1);
        assert_eq!(insp.schema_meta["hq_memory_sync_schema"], 3);
        assert!(insp.fts_present && insp.unicode_text_column);
        assert_eq!(insp.wrongmemory_extension, Some(1));
        // Baseline: the revision row appears on the first LIKE refresh, not at creation.
        assert!(insp.text_index_revisions.is_empty());
    }

    #[test]
    fn remember_writes_sage_projections_edges_and_hq() {
        let (_t, mut store) = managed();
        let id = remember(&mut store, "  Build with   CARGO ", None);
        let v = Viewer::default();
        let rec = store.get(&id, &v).unwrap().unwrap();
        assert_eq!(rec.text, "Build with CARGO");
        assert_eq!(rec.revision, 1);
        assert_eq!(rec.extra["wrongmemory"]["principalId"], json!("host"));
        let conn = Connection::open(store.path()).unwrap();
        let (canonical, unicode, legacy, tags): (String, String, String, String) = conn
            .query_row(
                "SELECT canonical_text, unicode_text, legacy_scope, tags FROM memories WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(canonical, "build with cargo");
        assert_eq!(unicode, "build with cargo");
        assert_eq!(legacy, "project-memory");
        assert_eq!(tags, "[]");
        let mut edges: Vec<(String, String, String)> = conn
            .prepare("SELECT from_node, to_node, relation FROM edges ORDER BY 1, 2")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        edges.sort();
        assert!(edges.contains(&("dir:src/app".into(), "dir:src".into(), "related_to".into())));
        assert!(edges.contains(&(
            "file:src/app/main.rs".into(),
            "dir:src/app".into(),
            "related_to".into()
        )));
        assert!(edges.contains(&(
            format!("mem:{id}"),
            "file:src/app/main.rs".into(),
            "about_file".into()
        )));
        let hq: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM hq_memory_sync WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hq, 1, "shared-scope insert feeds the HQ projection");
    }

    #[test]
    fn search_respects_session_visibility_before_limit() {
        let (_t, mut store) = managed();
        let public = remember(&mut store, "deploy uses blue green rollout", None);
        let private = remember(&mut store, "deploy secret session plan", Some("s1"));
        let none = Viewer::default();
        let s1 = Viewer {
            session_id: Some("s1".into()),
        };
        let s2 = Viewer {
            session_id: Some("s2".into()),
        };
        let ids = |v: &Viewer| -> Vec<String> {
            store
                .search("deploy", 10, None, None, v)
                .unwrap()
                .hits
                .into_iter()
                .map(|h| h.memory.id)
                .collect()
        };
        assert_eq!(ids(&none), vec![public.clone()]);
        assert_eq!(ids(&s2), vec![public.clone()]);
        let mut both = ids(&s1);
        both.sort();
        let mut want = vec![public, private.clone()];
        want.sort();
        assert_eq!(both, want);
        // Exact-ID: hidden and absent are indistinguishable.
        assert!(store.get(&private, &s2).unwrap().is_none());
        assert!(store.get("01NOPE", &s2).unwrap().is_none());
        assert!(store.get(&private, &s1).unwrap().is_some());
    }

    #[test]
    fn unicode_search_matches_baseline_channels() {
        let (_t, mut store) = managed();
        let id = remember(&mut store, "Çalışma ﬁle notları ① ve daha fazlası", None);
        let v = Viewer::default();
        // FTS5 unicode61 folds case/diacritics.
        let hits = store.search("çAlışma", 5, None, None, &v).unwrap();
        assert_eq!(hits.channel, "fts");
        assert_eq!(hits.hits[0].memory.id, id);
        // Baseline: FTS does not NFKC-fold, so `file` misses the ligature.
        let hits = store.search("file", 5, None, None, &v).unwrap();
        assert_eq!(hits.channel, "fts");
        assert!(hits.hits.is_empty());
        // A query with no usable FTS term uses the NFKC-folded LIKE channel.
        let hits = store.search("1", 5, None, None, &v).unwrap();
        assert_eq!(hits.channel, "like");
        assert_eq!(hits.hits[0].memory.id, id);
        // AND first, OR retry when AND finds nothing.
        let hits = store
            .search("notları zzzunknown", 5, None, None, &v)
            .unwrap();
        assert_eq!(hits.hits.len(), 1);
    }

    #[test]
    fn managed_store_ddl_matches_oracle_fingerprint() {
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../contracts/profiles/sage-sqlite-v5-baseline/manifest.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sage.db");
        SageStore::create_managed(&path, NOW).unwrap();
        let conn = Connection::open(&path).unwrap();
        let rows: Vec<serde_json::Value> = conn
            .prepare(
                "SELECT type, name, tbl_name, sql FROM sqlite_master
                  WHERE name NOT LIKE 'sqlite_%' AND name NOT LIKE 'wm_%'
                  ORDER BY type, name",
            )
            .unwrap()
            .query_map([], |r| {
                let sql: Option<String> = r.get(3)?;
                Ok(json!({
                    "type": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "tbl_name": r.get::<_, String>(2)?,
                    "sql": text::collapse_js_space(sql.as_deref().unwrap_or("")),
                }))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        use sha2::Digest;
        let digest = hex::encode(sha2::Sha256::digest(
            serde_json::to_string(&rows).unwrap().as_bytes(),
        ));
        assert_eq!(
            digest,
            manifest["ddlFingerprint"]["value"].as_str().unwrap(),
            "managed-store DDL diverges from the SAGE oracle"
        );
        let meta: std::collections::BTreeMap<String, i64> = conn
            .prepare("SELECT key, value FROM schema_meta")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let want: std::collections::BTreeMap<String, i64> =
            serde_json::from_value(manifest["schemaMeta"].clone()).unwrap();
        assert_eq!(meta, want);
    }

    #[test]
    fn idempotent_remember_replays_and_rejects_payload_change() {
        let (_t, mut store) = managed();
        let p = RememberParams {
            text: "retry me".into(),
            ..Default::default()
        };
        let a = store.remember(&p, &ctx(None, Some("k1"))).unwrap();
        let b = store.remember(&p, &ctx(None, Some("k1"))).unwrap();
        assert!(!a.replayed && b.replayed);
        assert_eq!(a.id, b.id);
        let n: i64 = Connection::open(store.path())
            .unwrap()
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "a replay never creates a duplicate memory");
        let other = RememberParams {
            text: "different".into(),
            ..Default::default()
        };
        let err = store.remember(&other, &ctx(None, Some("k1"))).unwrap_err();
        assert_eq!(err.code, ErrorCode::IdempotencyMismatch);
    }

    #[test]
    fn propose_creates_pending_candidate_and_checks_target() {
        let (_t, mut store) = managed();
        let target = remember(&mut store, "old fact", None);
        let hidden = remember(&mut store, "private", Some("s1"));
        let p = ProposeParams {
            text: "better fact".into(),
            target_memory_id: Some(target.clone()),
            target_revision: Some(1),
            suggested_action: Some("update".into()),
            ..Default::default()
        };
        let out = store.propose(&p, &ctx(None, None)).unwrap();
        assert_eq!(out.outcome, "candidate-pending");
        let (list, _, corrupt) = store
            .list_candidates("pending", 10, &Viewer::default())
            .unwrap();
        assert_eq!(corrupt, 0);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].target_memory_id.as_deref(), Some(target.as_str()));
        // Stale revision and hidden targets are refused.
        let stale = ProposeParams {
            target_revision: Some(7),
            ..p.clone()
        };
        assert_eq!(
            store.propose(&stale, &ctx(None, None)).unwrap_err().code,
            ErrorCode::RevisionConflict
        );
        let probe = ProposeParams {
            target_memory_id: Some(hidden),
            target_revision: None,
            ..p
        };
        assert_eq!(
            store
                .propose(&probe, &ctx(Some("s2"), None))
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }

    #[test]
    fn corrupt_row_is_reported_and_siblings_still_return() {
        let (_t, mut store) = managed();
        let good = remember(&mut store, "corruption neighbor survives", None);
        Connection::open(store.path())
            .unwrap()
            .execute(
                "INSERT INTO memories (id, data, status, kind, scope, importance, confidence,
                   freshness, updated_at, created_at, canonical_text)
                 VALUES ('bad', '{not json corruption', 'active', 'fact', 'project', 1, 1, 1,
                   '2026', '2026', 'corruption')",
                [],
            )
            .unwrap();
        let r = store
            .search("corruption", 10, None, None, &Viewer::default())
            .unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].memory.id, good);
        // The guarded trigger indexed the malformed row with empty text, so it
        // is not an FTS hit at all; the LIKE path reports it as corrupt.
        let r = store
            .search("", 10, None, None, &Viewer::default())
            .unwrap();
        assert_eq!(r.channel, "recent");
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.corrupt_skipped, 1);
        // The LIKE channel refreshes unicode_text (corrupt row folds to '').
        let r = store
            .search("∅ corruption", 10, None, None, &Viewer::default())
            .unwrap();
        assert_eq!(r.channel, "fts");
        let insp = store.inspect().unwrap();
        assert!(insp.text_index_revisions.is_empty());
        let r = store
            .search("①", 10, None, None, &Viewer::default())
            .unwrap();
        assert_eq!(r.channel, "like");
        assert!(r.hits.is_empty());
        assert_eq!(
            store.inspect().unwrap().text_index_revisions["memories.unicode_text"],
            "nfkc-lower-v1"
        );
    }

    #[test]
    fn for_path_matches_exact_and_ancestor_anchors() {
        let (_t, mut store) = managed();
        let id = remember(&mut store, "main entry point note", None);
        let v = Viewer::default();
        assert_eq!(
            store.for_path("src/app/main.rs", 10, &v).unwrap().hits[0]
                .memory
                .id,
            id
        );
        // Baseline retrieveForPath: a directory query does not return memories
        // anchored to files below it; a file query does return memories
        // anchored to its ancestor directories.
        assert!(store.for_path("src\\app", 10, &v).unwrap().hits.is_empty());
        let dir_note = store
            .remember(
                &RememberParams {
                    text: "Everything under src/app is generated code".into(),
                    anchors: vec![MemoryAnchor {
                        kind: "directory".into(),
                        path: Some("src/app".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                &ctx(None, None),
            )
            .unwrap()
            .id;
        let ids: Vec<String> = store
            .for_path("src/app/other.rs", 10, &v)
            .unwrap()
            .hits
            .into_iter()
            .map(|h| h.memory.id)
            .collect();
        assert_eq!(ids, vec![dir_note]);
        assert!(
            store
                .for_path("docs/readme.md", 10, &v)
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[test]
    fn read_only_store_refuses_mutation_and_does_not_change_bytes() {
        let (tmp, mut store) = managed();
        remember(&mut store, "fixture", None);
        let path = store.path().to_path_buf();
        drop(store);
        // Checkpoint so the main file is self-contained, then hash it.
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let (mut ro, _) = SageStore::open(&path, false, Some(tmp.path().into()), NOW).unwrap();
        assert_eq!(
            ro.search("fixture", 5, None, None, &Viewer::default())
                .unwrap()
                .hits
                .len(),
            1
        );
        let p = RememberParams {
            text: "nope".into(),
            ..Default::default()
        };
        assert_eq!(
            ro.remember(&p, &ctx(None, None)).unwrap_err().code,
            ErrorCode::ReadOnly
        );
        drop(ro);
        assert_eq!(before, std::fs::read(&path).unwrap());
    }

    #[test]
    fn inspector_refuses_newer_and_foreign_databases() {
        let tmp = tempfile::tempdir().unwrap();
        let newer = tmp.path().join("newer.db");
        SageStore::create_managed(&newer, NOW).unwrap();
        Connection::open(&newer)
            .unwrap()
            .execute("UPDATE schema_meta SET value = 6 WHERE key = 'version'", [])
            .unwrap();
        assert_eq!(inspect(&newer).verdict, Verdict::Newer);
        let err = SageStore::open(&newer, false, None, NOW).err().unwrap();
        assert_eq!(err.code, ErrorCode::ProfileUnsupported);

        let foreign = tmp.path().join("foreign.db");
        Connection::open(&foreign)
            .unwrap()
            .execute_batch("CREATE TABLE t(x);")
            .unwrap();
        assert_eq!(inspect(&foreign).verdict, Verdict::NotASageStore);
        let junk = tmp.path().join("junk.db");
        std::fs::write(&junk, b"definitely not sqlite, just bytes padding padding").unwrap();
        assert!(!inspect(&junk).verdict.readable());
        assert_eq!(
            inspect(&tmp.path().join("none.db")).verdict,
            Verdict::Missing
        );
    }

    /// Known answers generated by the oracle from the real SAGE functions.
    #[test]
    fn text_functions_match_oracle_known_answers() {
        let shape: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../contracts/fixtures/record-shape.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let kats = &shape["kats"];
        let mut checked = 0;
        for (name, f) in [
            ("normalizeText", text::normalize_text as fn(&str) -> String),
            ("normalizeTextKey", text::normalize_text_key),
            ("unicodeTextFold_nfkc_lower_v1", text::unicode_fold),
        ] {
            for case in kats[name].as_array().unwrap() {
                let input = case["input"].as_str().unwrap();
                assert_eq!(
                    f(input),
                    case["output"].as_str().unwrap(),
                    "{name}({input:?})"
                );
                checked += 1;
            }
        }
        for (name, f) in [
            ("tokenize", store::tokenize as fn(&str) -> Vec<String>),
            ("ftsPrefixTerms", store::fts_prefix_terms),
        ] {
            for case in kats[name].as_array().unwrap() {
                let input = case["input"].as_str().unwrap();
                let want: Vec<String> = serde_json::from_value(case["output"].clone()).unwrap();
                assert_eq!(f(input), want, "{name}({input:?})");
                checked += 1;
            }
        }
        assert!(checked > 40, "KAT corpus unexpectedly small: {checked}");
    }
}
