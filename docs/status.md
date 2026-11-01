# Implementation status

Last updated: 2026-11-01 (session 1). **Release level reached: none.** L1 needs a bounded compatibility backend for the full SAGE profile, and L2 needs the WrongStack adapter.

## Baseline

- **WrongStack commit:** `222d23a90ae9c67c2433d7734c6966be7ee4e696` ([manifest](../contracts/baseline/wrongstack-baseline.json)). It covers 202 contract files, none of them dirty.
- **Drift since the plan:** the plan was written against `ad9f79d`. No contract file changed between the two commits.
- **Oracle source:** a `git archive` of that commit, rebuilt outside the repository (`C:/wrongmemory-oracle-src/...`).
- **Fixture location:** fixtures are built under `C:/wrongmemory-oracle/<fixture>`, so the committed databases contain no user paths.
- **Storage profile:** `sage-sqlite-v5-baseline` ([manifest](../contracts/profiles/sage-sqlite-v5-baseline/manifest.json), [schema.sql](../contracts/profiles/sage-sqlite-v5-baseline/schema.sql)).
  - DDL fingerprint: `d8e36f92…090b`.
  - Fresh `schema_meta`: `version=5`, `fts_index_initialized=1`, `hq_memory_sync_schema=3`.
- **SQLite versions:** Bun 3.53.4; bundled rusqlite 0.41.1.

## M0 — contract capture

| ID | State | Evidence / gap |
|---|---|---|
| WM-000 | Done | `scripts/baseline/export.ts`: hashes taken from `HEAD` blobs, and the dirty-file list is empty. |
| WM-001 | Done (seed) | Full `sqlite_master` from a store SAGE created: 10 triggers, plus the FTS shadow tables, HQ tables and Unicode helper. Covered by `managed_store_ddl_matches_oracle_fingerprint`. |
| WM-002 | Done (seed) | `contracts/fixtures/record-shape.json` holds the record fields, every vocabulary, input limits and the `rememberSage` defaults. It also holds known answers from the real functions: `normalizeText`, `normalizeTextKey`, `unicode_text` fold, `tokenize`, `ftsPrefixTerms` and `sqliteNormalizeCommand`. |
| WM-003 | Done (review list) | `contracts/inventory/writers.json`: 1,224 name-matched sites, 684 of them flagged as suspected collisions. It is name-based; confirm each site before WM-400. |
| WM-004 | Partial | Fixtures `empty`, `basic`, `unicode` and `corrupt-row`, each with a recorded `queries.json` (151 operations). Not yet captured: cursors, ranking for queries with two or more terms, graph traversal parity, audience retrieval. |
| WM-005..011 | Not started | Mutation, replay and failure fixtures; MCP tool inventory; corpora; executing the existing SAGE suites; performance baseline; license review. |

## M1 — standalone repository and safe storage slice

| ID | State | Evidence / gap |
|---|---|---|
| WM-100 | Done (local) | Cargo workspace, toolchain pin 1.93.0, Bun scripts, `AGENTS.md`, Windows CI workflow (hosted run 41300045407: PASS). |
| WM-101 | Partial | Exact pins from crates.io: `rusqlite 0.41.1`, `tokio 1.54.1`, `rmcp 3.8.0`, `ulid 3.0.0`, `unicode-normalization 0.1.26`. Toolchain upgrade and MSRV review: NOT RUN. |
| WM-102 | Partial | Rust contracts with unknown-field round-trip tests. Generated TypeScript DTOs: not started. |
| WM-103 | Done | Read-only inspector with verdicts compatible, unknown-data, older, newer, divergent, not-a-sage-store, unreadable and missing. A test proves the adopted store's bytes are unchanged. |
| WM-104 | Partial | Native DDL, codec, FTS and Unicode text index (see the search parity section). |
| WM-105 | Done | `registry.db` with migrations; a newer schema is refused. Private data root with a protected DACL (shared with WrongIndex). |
| WM-106 | Done | Singleton through the first pipe instance: 8 concurrent starts leave exactly one survivor, and the losers exit with code 3. Authentication uses host token or grant secret; frame limits are 4 MiB, auth frames 16 KiB with a 5 s auth deadline, and at most 64 connections. |
| WM-107 | Partial | `OwnerGuard` exclusive lock file plus owner epochs and an activation/release journal in the registry. A transfer protocol with WrongStack is not implemented. Adoption is refused (read-only) while `server.json` exists. |
| WM-108 | Done (slice) | `wm_mutation_receipts` is an additive extension (`wrongmemory-ext-v1`). The receipt and the mutation commit in one IMMEDIATE transaction. Replay survives a daemon restart, and reusing a key with a different payload returns `IDEMPOTENCY_MISMATCH`. No outbox yet. |
| WM-109 | Done | CLI commands: `daemon start/status/stop`, `project add/list/show/detach`, `search`, `get`, `for-path`, `remember`, `candidates`, `grants`, `inspect`, `doctor`, `connect print`. |

## M2 / M3 work pulled forward (slice only)

- **WM-200 / WM-201 (partial):** project registration takes one of two store kinds.
  - Adopted: `<root>/.wrongstack/memories/sage.db` in place.
  - Managed: `<data-root>/projects/<id>/sage.db`.
  - Registration is idempotent. A second authoritative store for the same root is refused.
  - Linked-worktree canonical identity is not ported yet. Register the canonical root.
- **WM-202 / WM-203 (partial):**
  - Grants carry a profile and a project allowlist, and their secrets are stored hashed.
  - A grant connection always runs in a fresh daemon-issued session. Only the host principal may bind a named session.
  - Revocation is checked on every request.
- **WM-204 (partial):** one session rule (SAGE `buildSessionClause` without the `includeAllSessions` bypass) applies to search, exact ID, path and candidates. Graph, count and cursor surfaces do not exist yet.
- **WM-301 / WM-302 / WM-303 (partial):**
  - `wrongmemory mcp` stdio bridge on `rmcp 3.8.0`. It starts the daemon itself, so WrongStack does not need to run.
  - Tools: `memory_search`, `memory_for_path`, `memory_get`, `memory_candidates` (list and propose), and `remember` for writer grants only.
  - The tool list is derived from the grant profile, and the daemon enforces every action anyway.
  - Bridge EOF leaves the daemon running.
- **WM-305..309:** NOT RUN. No named client preset was installed or edited; `connect print` only prints an entry.

## Search parity (WM-500 slice)

Ported from `searchSage` / `ftsPrefixTerms`:

- query terms split on whitespace and then on `[^\p{L}\p{N}_]`, minimum length 2, quoted prefix terms, at most 64 terms;
- FTS MATCH with AND first, then OR when AND finds nothing and there is more than one term;
- `bm25 ASC, importance DESC` ordering;
- `unicode_text LIKE` fallback with the lazy `nfkc-lower-v1` refresh, on writable handles only;
- an empty query sorts by importance, then recency;
- `contextPolicy=never` is excluded when the caller passes no statuses.

`crates/wrongmemory-storage/tests/oracle_parity.rs`:

- Search order matches the oracle for every single-term query in the four fixtures. For queries with two or more terms only the result set is compared.
- Exact-ID records match field by field, including unknown fields.
- Corrupt rows read as absent.
- Path hits are a subset of the oracle's.

**Known gaps (not claimed):**

- `hybridRerankMemories` re-ranking for queries with two or more terms.
- `retrieveForPath` graph expansion.
- The remember-time `ephemeral_pattern` / `progress_log` quality caps.
- Near-duplicate merging and contradiction detection.
- Update, delete, recovery, feedback and hygiene.
- `\p{L}` is approximated by `char::is_alphabetic`. The known answers pass, but Other_Alphabetic marks can differ.

## Findings

- **Fixed: a spawned daemon kept the caller's stdio pipes open.** On Windows, a daemon started by the CLI or bridge inherited the caller's stdio handles. The coding tool's stdout pipe then never reached EOF. The bridge test caught it; the fix marks the std handles non-inheritable before spawning.
- **Baseline: FTS does not apply NFKC.** `file` does not match `ﬁle` through FTS; only the LIKE channel folds NFKC. Preserved.
- **Baseline: legacy unowned session rows.** Session-scoped rows with no owner are visible to callers without a session. Grant connections always have a session, so they never see them.
- **Baseline: corrupt rows.** `getSage` on a corrupt row returns null. Corrupt rows still occupy pagination slots in `listSagePage`.
- **Not baseline: attribution fields.** WrongMemory adds `wrongmemory.principalId` (memories) and `proposedBy` (candidates) as extension JSON fields. SAGE readers keep unknown JSON fields, but this has not been verified by reopening with SAGE (NOT RUN).

## Verification run (2026-11-01, Windows 11, local)

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo test --workspace --locked` ×5 | PASS 50/50 each run; no leftover daemon processes |
| Oracle scripts (`baseline:export`, `oracle:prepare`, `oracle:build`, `inventory:writers`) | PASS; generated JSON is byte-identical across two runs (the sage.db bytes differ because of ULID and randomblob) |
| Real MCP client (Claude Code / Codex / Cursor / Antigravity) | NOT RUN |
| WrongStack adapter, ownership transfer, live store adoption | NOT RUN (not implemented) |
| Hosted CI (windows-latest, run 41300045407) | PASS |

## Next exit gates

1. **M0 completion:** mutation, replay and failure fixtures; capture of the multi-term rerank and of `retrieveForPath`; the MCP tool inventory from `sage-mcp`.
2. **WM-107 / WM-400:** cooperative ownership transfer with a WrongStack adapter version, then the `WrongMemoryPort` slice.
3. **WM-500..505:** update/feedback, delete, recovery and candidate accept/resolve, with revision checks.
4. Named client smoke tests (WM-306) on installed clients.
