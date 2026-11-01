# Implementation status

Last updated: 2026-11-01 (session 2).

**Release level reached: none certified.**

- The standalone daemon, IPC, policy, MCP bridge, native SAGE-profile storage and the WrongStack adapter branch are implemented and tested.
- Not yet run, so nothing is certified:
  - real installed MCP clients;
  - live store adoption;
  - a signed hosted release;
  - a demonstration with a downloaded binary.

Legend: **Done**, meaning implemented and covered by an automated test or recorded command; **Partial**; **Not started**; **NOT RUN**, meaning implemented but never exercised in a real external environment.

## Baseline

- **WrongStack commit:** `222d23a90ae9c67c2433d7734c6966be7ee4e696` ([manifest](../contracts/baseline/wrongstack-baseline.json)). It covers 202 contract files, none of them dirty.
- **Storage profile:** `sage-sqlite-v5-baseline` with DDL fingerprint `d8e36f92…090b`. The managed store matches it exactly.
- **Extension `wrongmemory-ext-v1`:** adds the tables `wm_mutation_receipts` and `wm_profile_extensions`.
- **Oracle:** fixtures are produced by running the real SAGE code at the baseline commit (`scripts/oracle`).
  - Read fixtures: `empty`, `basic`, `unicode` and `corrupt-row`.
  - Mutation fixtures: 7 scenarios with 188 recorded steps.
  - Fake secrets in the fixtures are redacted when generated.

## M0 — contract capture

| ID | State | Evidence / gap |
|---|---|---|
| WM-000 | Done | `scripts/baseline/export.ts` |
| WM-001 | Done | Full DDL, triggers, FTS and HQ tables. Test: `managed_store_ddl_matches_oracle_fingerprint`. |
| WM-002 | Done | `contracts/fixtures/record-shape.json`, with known answers from the real normalizers. |
| WM-003 | Done (review list) | `contracts/inventory/writers.json`. It matches by name, so the suspected collisions are not individually confirmed. |
| WM-004 | Done | Search with one or more terms, path retrieval, exact ID, cursors, graph, related and audience retrieval, recorded in the oracle fixtures (`oracle-mutations/graph-retrieval.json`, `audience.json`). |
| WM-005 | Done | Mutation fixtures: remember/merge, update/revision, feedback, delete/recover, candidates, hygiene. |
| WM-006 | Done | `contracts/inventory/mcp-tools.json` (`sage-mcp` tool and policy inventory). |
| WM-007 | Partial | Synthetic corpora: multilingual (`unicode`), worktree (temp git repos), session (adversarial tests). No real-task corpus. |
| WM-008 | Partial | The existing SAGE suites run green in the adapter worktree (see M4). The SAGE vector and host suites have no WrongMemory counterpart. |
| WM-009 | Done | `contracts/profiles/` |
| WM-010 | Done (synthetic) | `wrongmemory-bench` (see Performance). No baseline taken from SAGE running in process. |
| WM-011 | Done | The repository is MIT. `scripts/inventory/licenses.ts` writes `contracts/inventory/licenses.json`: 143 third-party crates, every one with a permissive SPDX alternative. CI fails on any crate without one. The TS client has no runtime dependencies. Model licenses are not applicable (no models ship). |

## M1 — repository and safe storage

| ID | State | Evidence / gap |
|---|---|---|
| WM-100 | Done | Workspace, toolchain 1.93.0, `AGENTS.md`, Windows CI. |
| WM-101 | Done | Every workspace dependency is pinned exactly (`getrandom` was the last loose one, now `=0.3.4`). MSRV is declared as `rust-version = "1.93"`, equal to the pinned toolchain; older toolchains are not supported or tested. |
| WM-102 | Done | Rust contracts. `packages/client-ts` mirrors them; `scripts/check-contract-parity.ts` passes (38 methods, 22 error codes). |
| WM-103 | Done | Read-only inspector. A byte-unchanged test proves it writes nothing. |
| WM-104 | Done | Native DDL, codec, FTS and Unicode projection. |
| WM-105 | Done | Registry migrations v1→v2 (jobs and schedules); a newer schema is refused; private DACL. |
| WM-106 | Done | Singleton, owner-only pipe, 4 MiB / 16 KiB frame limits, auth deadline. The daemon proves itself with HMAC before any credential is sent (Rust and TS, with a squatting test). |
| WM-107 | Partial | `OwnerGuard` lock, epochs, journal. No cooperative transfer protocol with a running WrongStack. |
| WM-108 | Done | Receipts commit in the same transaction as the mutation; replay survives a restart and a crash (WM-602). There is no outbox; HQ sync is not implemented. |
| WM-109 | Done | CLI: daemon, project, grants, search/get/for-path/remember, candidates, inspect, doctor, connect, backup, migrate, diagnostics, install. |

## M2 — projects, policy, lifecycle

| ID | State | Evidence / gap |
|---|---|---|
| WM-200 | Done | Adopted and managed stores; idempotent; a second store for one root is refused. |
| WM-201 | Done | A linked worktree resolves to the main checkout store (`tests/worktree.rs`, using the ported `resolveProjectSageStorageRoot` logic). |
| WM-202 | Done | Grant profiles read-only / read-propose / writer / curator, project allowlist, hashed secrets, revocation checked on every request. |
| WM-203 | Done | Sessions issued by the daemon for grants; only the host may bind or resume a named session. |
| WM-204 | Done | One session rule applies before LIMIT on search, ID, path, list cursor, related, graph, stats, candidates and events (`tests/adversarial.rs`). Known gap: see WM-603. |
| WM-205 | Done | One store mutex per project acts as its serialized writer; SQLite runs on the blocking pool. LRU of open stores, capped at 32. |
| WM-206 | Done | Durable maintenance schedules and jobs in the registry; tick-driven scheduler; cancel. |
| WM-207 | Done | `events.since`, filtered by visibility. |
| WM-208 | Done | Graceful shutdown, restart over a stale endpoint, crash recovery, and three injected failures (`tests/fault.rs`):<ul><li>Disk full (a real `SQLITE_FULL` through a debug-only `max_page_count` cap): a typed `DISK_FULL`, reads keep working, no row or receipt is left, and the same idempotency key commits once space is back.</li><li>A store that is missing after a restart: a typed error, other projects unaffected, and it recovers without a restart once the files are restored.</li><li>A store that is corrupt after a restart: the same as a missing store.</li></ul>Deleting a store while it is open is not possible on Windows. |

## M3 — MCP

| ID | State | Evidence / gap |
|---|---|---|
| WM-300 | Done | rmcp 3.8.0 stdio. |
| WM-301 | Done | `wrongmemory mcp` starts the daemon detached, so the caller's stdio handles are not inherited. |
| WM-302 | Done | Read tools: `memory_search`, `memory_for_path`, `memory_get`, `memory_graph`, `memory_gather_batch`, `memory_related`. |
| WM-303 | Done | `memory_candidates` and `memory_feedback`. |
| WM-304 | Done | Writer and curator tools, listed only for grants that allow them; the daemon enforces every action anyway. |
| WM-305 | Done | Planner for JSON and TOML client configs, with owned-entry markers; tests write only to temp prefixes. |
| WM-306 | **NOT RUN** | Presets for Claude Code, Codex, Cursor and Antigravity are implemented and tested on temp configs. No installed client has been connected. |
| WM-307 | Done | `connect print`, `client doctor`, `disconnect` (with grant revocation). |
| WM-308 | Not started (deferred) | Loopback HTTP transport. |
| WM-309 | **NOT RUN** | Evaluating an external agent workflow. |

## M4 — WrongStack adapter

Branch `feat/wrongmemory-adapter` in the worktree `D:/Codebox/PROJECTS/WrongStack-wrongmemory`. It is local and not pushed.

- `478fa2333`: the adapter.
- `c13cf9657`: re-copies the client at `40f7275` to pick up the HMAC proof fix.
- `49eb02d39`: `retrieveForAudience` through `memory.forAudience`.
- `fa00a83a4`: legacy `forget`, `consolidate` and `clear` through `memory.legacy*`.
- `a9de6daed`: `findMemoriesForFile` through `memory.forFile`.
- `3ee3ca973`: `verify` and `verifyForPaths` through `memory.verify`.
- `488e07e8d`: `unifiedSearchService` through `memory.unifiedSearch`, with an invalid cursor mapped to `InvalidSearchCursorError`. Adapter tests: 25/25 against the real daemon.
- `fff633d45`: `surface.importLegacy(files)` through `memory.importLegacy` (curator action). The client is re-vendored at `d78559e`, and the author field in its package.json is no longer mis-encoded. Adapter tests: 27/27 against the real daemon; sage and client typecheck and biome are clean.
- `5e46190b8`: client re-vendored at `1799ad0` (`PROJECT_EXPORT`), plus a WrongMemory → local SAGE → WrongMemory rollback round-trip test. Adapter tests: 28/28.

| ID | State | Evidence / gap |
|---|---|---|
| WM-400 | Done (branch) | `packages/sage/src/wrongmemory-port.ts`. Typed `UNAVAILABLE` errors; it never falls back to local SQLite. Port tests run against a real daemon. No operation throws `NOT_IMPLEMENTED` any more; unified search goes through `memory.unifiedSearch`. `searchSageWithBreakdown` is lexical-only because there is no vector channel (WM-405). |
| WM-401 | Done (branch) | `Sage.service` is `local` (the default), `wrongmemory` or `auto`; a repo's own config cannot set it. |
| WM-402 | Done (branch) | Session ids reach the port per call from the host middleware (`host-wiring`, tool-call and turn memory), and the port binds them with `session.acquire{hostSessionId}`. Like the baseline container, the WrongMemory port gets no container-level resolver. A call without a session releases the binding, so it under-reports and cannot leak. |
| WM-403 | Partial | The daemon serves audience retrieval, `findMemoriesForFile`, `verify` and `verifyForPaths`. Session ids follow the baseline: the host passes them per call (the baseline container gives the in-process port no ambient `getSessionId` either), and the port binds them through `session.acquire`. Setup-time injection through a real host run remains open. |
| WM-404..408 | Not started | Capture/remap hooks; vector fusion; WebUI/slash/desktop; MCP connect migration; HQ sync. |
| WM-409 | Partial | `local` mode leaves behaviour unchanged. `auto` uses WrongMemory only if a daemon is already running and the project is registered. Ownership-safe rollback is not exercised. Rollback is exercised in the adapter (`5e46190b8`): write through WrongMemory, run `project.export`, read and write the exported store with local SAGE, then switch back to WrongMemory, which adopts the same store and sees both records. Still NOT RUN: a full WrongStack host run switching modes. |

Without the adapter, the WrongStack suites on the branch give core 10305/10305 pass. With it:

- sage 1564/1565: `sqlite-perf.test.ts` times out under full-suite load and passes on its own.
- runtime 401/403: both failures are in `automation-pull-request-status-sha.test.ts`, which needs the untracked `.temp_files` directory.

These failures are not caused by the adapter. The main checkout was not modified.

## M5 — retrieval and lifecycle parity

| ID | State | Evidence / gap |
|---|---|---|
| WM-500 | Done | Search order is identical to the oracle for every query with one or more terms, including the hybrid rerank (`tests/oracle_parity.rs`). `\p{L}` is approximated by `char::is_alphabetic`. |
| WM-501 | Done | `tests/oracle_retrieval_replay.rs` replays the oracle's graph-retrieval and audience scenarios and compares results. Path, related, graph, traverse, audience (including the truncation audit), search, list paging and stats `byStatus`/`byKind` all match. This replay found and fixed three gaps: `graphFor` now adds the anchors of every memory start, and `findRelated` is a full port of the baseline candidate collection (anchor-path ancestors, about_* targets, importance-ordered tags, command families, final ranking). `memory.forAudience` is exposed over IPC. `findMemoriesForFile` is `memory.forFile` plus the MCP tool `memory_for_file`. It matches the oracle bucket by bucket and uses a ported `normalizeProjectPath` with realpath containment. Supersession heads and review hints are resolved among visible records only. |
| WM-501b | Done | `unifiedSearchService` is `memory.unifiedSearch`. It covers filters, audience and anchor post-filters, four rankings, absolute scores, lexical suggestions and strict keyset cursors bound to the query, filters, ranking and session. The replay of the oracle's `unified-search` scenario (54 steps) matches order, scores, bm25, cursor payloads including the binding hash, totals, echo and refusals. The session comes from the connection, and client `sessionId` and `includeAllSessions` are dropped. |
| WM-502 | Done | Remember, exact and near-duplicate merge, supersedes and contradicts, raw-JSON patch on update, revision guard (lifecycle oracle replay). |
| WM-503 | Done | Feedback (up to 8 entries; session and principal set by the server), usage counters. |
| WM-504 | Done | Candidates: propose with dedupe; accept, reject and resolve in one transaction; reverted resolution writes an audit record. Accepting a consolidation proposal supersedes its members. Legacy `consolidate(scope)` is `memory.legacyConsolidate` (see WM-510). |
| WM-505 | Done | Tombstone, cascade, recover, backfill. Tombstone purge runs through hygiene (`purgeDeletedAfterDays`), and the oracle's `hygiene-purge` step matches. |
| WM-506 | Done (with deviations) | `memory.verify` (`{id}` or `{paths}`) and the curator MCP tool `memory_verify`. A replay of the oracle's `verify` scenario matches verdicts, reason strings, content hashes and the reconciled state, including manual stale kept, reactivation and per-path selection. Nothing is executed. Deviations:<ul><li>Git blob ids are computed natively, and a mismatch on a file containing CR is reported `unknown`.</li><li>Command anchors are probed by `stat` on the daemon's `PATH`, which can differ from the client's.</li><li>Explicit command paths are probed only inside the project root; anything else is `unknown`.</li><li>UNC, verbatim and device paths, and absolute paths outside the root, are rejected without touching the filesystem. This applies everywhere a path is resolved: verify, hygiene, `forFile`, verify-by-paths.</li><li>Only rows visible to the caller's session are verified.</li></ul>Checkout evidence beyond anchors is not implemented. |
| WM-507 | Done | Hygiene with protections for critical memories (oracle replay). Expiry is scheduled through jobs. |
| WM-508 / 509 | Not started (optional L4) | Semantic provider and vector mirror. |
| WM-510 | Partial | Corrupt rows read as absent. Legacy `MemoryStore` bulk mutations are curator-only, and the replay of the oracle's `legacy-bulk` scenario matches state, merged anchors and sources, counts and audit details:<ul><li>`memory.legacyForget`</li><li>`memory.legacyConsolidate`</li><li>`memory.legacyClear` (with no scope it needs `all=true`)</li></ul>Deliberate deviation: these only touch rows the caller's session can see, so one session cannot erase another session's private memories. `consolidate` also groups session rows only with rows of the same owning session, so a merge cannot copy private content or ids into shared rows. Legacy Markdown notes import through `memory.importLegacy` (`importLegacyFiles` parity: bullet lines become project memories, files must stay inside the project root after realpath, 5 MiB total cap enforced on the bytes read, UNC paths are never touched; Curate action). Deliberate deviations after a security review: only regular `.md`/`.markdown` files are read, nothing under `.wrongstack/` (an adopted store, its WAL and journal) or `.git/`, and a file that is not valid UTF-8 is refused instead of decoded lossily, so the import cannot copy another session's private rows out of the store's bytes. Older and JSONL *stores* are refused: open them with SAGE once first, which converts them. There is no in-place conversion. |
| WM-511 | Not started | Full native differential certification. |

## M6 — hardening

| ID | State | Evidence / gap |
|---|---|---|
| WM-600 | Partial | Online backup API (`wrongmemory backup`) on copies. Adopting, transferring or releasing a real store is **NOT RUN**, as required. |
| WM-601 | Done (local) | A conflict between a managed and an adopted store for one root is refused. `project.export` / `wrongmemory project export <id>` (host only) hands a managed project back to in-process SAGE:<ul><li>Under the project's writer lane it snapshots the store into `<root>/.wrongstack/memories/sage.db`, checks `integrity_check` and the read-only inspector verdict, then retires the handle (a request already waiting for the lane gets `PROJECT_NOT_FOUND`) and detaches the registration.</li><li>No write can reach the managed copy after the snapshot, and the managed files are kept.</li><li>An existing `sage.db`, `-wal`, `-shm` or `-journal` is refused with `STORE_IDENTITY_CONFLICT`, with nothing written or detached. Adopted stores are refused.</li></ul>`managed_store_exports_to_the_legacy_location_and_detaches` checks the copy (Compatible, 2 rows, adoptable and searchable), the detach, the conflict, the adopted refusal and the curator `FORBIDDEN`. Not exercised: a real WrongStack opening the exported copy. |
| WM-602 | Done | Fault points (debug builds only) `after_commit_before_reply` and `before_commit`. Retry replays the receipt, rollback leaves nothing behind, a crash during accept recovers, and a kill while idle restarts over a stale endpoint. Crash cases were repeated 5×. |
| WM-603 | Done (with gaps) | See the review results below. |
| WM-604 | Done (local) | `wrongmemory-bench --soak-secs N` runs mixed search, forPath, propose, remember and listPage traffic with 8 clients across 40 projects. That is more than the cap of 32 open stores, so stores are evicted and reopened under load. Result: `benchmarks/results/2026-11-01-soak.json`.<ul><li>**Found and fixed:** concurrent requests for a closed project raced on its owner lock. 9% of requests (48,868 of 523,202) failed with `OWNER_CONFLICT` against the daemon's own lock. Store opens are now serialized behind one opening lock (cache hits are unaffected), and a regression test fails without the fix.</li><li>**After the fix (5 min):**<ul><li>260,142 operations, 0 errors.</li><li>Median latency is 0.5–3.5 ms per operation. p95 is about 40 ms, from the deliberate eviction churn.</li><li>Handles 285 → 293 (no leak); RSS 34 → 86 MB with 32 stores open.</li></ul></li></ul>WAL tuning and multi-hour soaks have not been run. |
| WM-605 | Done (hand-written corpus) | `tests/fixtures/quality/corpus.json`: 40 project memories modeled on coding-agent memory (not real user data), each with a natural-language paraphrase query. It adds 5 hard negatives: superseded predecessors, a contradicted fact, and another session's private notes that match the queries lexically. `quality_budget.rs` enforces recall@1, recall@5 and MRR of at least 0.95 and allows zero negatives; measured 0.975 / 0.975 / 0.979. Not a held-out corpus of real tasks. |
| WM-606 | Done (local) | `concurrent_bridges_share_one_native_daemon` does a cold start: three MCP bridges launch at once on one data root, each spawning the daemon on demand. They end up served by exactly one native `wrongmemoryd`. The only other processes on that data root are the three disclosed bridges, with no Bun or Node engine. Bridges exit on EOF and the daemon stays up. Passed 5 of 5 runs locally, then failed once on hosted CI (run 41318993711, `d78559e`): `daemon stop` returns while the old daemon is still draining, so its discovery record outlives its endpoint.
  - A bridge that connected to the new daemon with the old token got "did not prove … no credential sent". `connect_grant` treated every message containing "credential" as final, so the bridge exited.
  - A start that lost to a draining daemon (whose pipe instances still hold the endpoint) was never retried, so the bridge timed out after 10 s.
  - Fix: only a missing grant secret or a remote authentication error is final. Transport and Protocol errors during the handshake also go through start-and-wait, which re-spawns every second until a daemon answers.
  - The new debug fault `slow_lifecycle` (1.5 s before publish and before retract) makes the window deterministic. `bridge_waits_out_a_stale_discovery_during_restart` fails without the fix (bridge exits, broken pipe) and passes with it, and the bridge suite passed 10 of 10 runs with no leftover daemon. An interim variant of that test leaked one daemon once; it was not seen again.
  - **Open (intermittent):** after the fix the test failed once more on hosted CI (run 41322866680, `122c275`, docs-only commit, again about 10 s into the test), then passed on `782a2a2` and three reruns of it. It has not failed locally in 10 sequential and 24 parallel runs. Since `782a2a2` a dying bridge reports its stderr and the daemon log in the test failure, so the next hosted failure will carry the cause. The WrongStack side (no SAGE server in `wrongmemory` mode) is covered by the adapter's port tests, not certified in a full host run. |

**WM-603 isolation review.** Four leaks were found and fixed, each with a test that failed first:

1. `list_candidates` applied visibility after LIMIT, so hidden rows could starve the visible ones.
2. `memory.stats` counts exposed rows the viewer cannot see.
3. `memory.graph` exposed private anchor symbols through the shared structural nodes.
4. `events.since` exposed the ids of private candidates.

Two gaps remain open:

- A directory node created only by a hidden memory's anchor path, and referenced by no memory, can still appear as a directory name in `memory.graph`.
- `stats.edges` now counts only memory-owned visible edges. This deliberately deviates from the SAGE count.

## M7 — packaging and operations

| ID | State | Evidence / gap |
|---|---|---|
| WM-700 | Done (local) | `install`, `uninstall`, `autostart enable/status/disable` (per-user Run value), tested with override keys and temp prefixes. |
| WM-701 | Partial | `install --stop-daemon` keeps `.old` copies for rollback; `migrate inspect`. The v1→v2 registry upgrade keeps projects, epochs and grants (`v1_registry_upgrades_in_place`). `scripts/upgrade/upgrade-smoke.ps1` exercises upgrades between built releases in a throwaway work directory (temp data roots, projects and install prefix; no PATH, autostart or real store). Run 2026-11-01, **PASS 11/11**:<ul><li>`40f7275` → `bab5b13` with `install --stop-daemon` over a running old daemon. The old fact, old grant and MCP bridge all work after the upgrade.</li><li>Rollback via `*.exe.old`: the old build reads data the new build wrote.</li><li>Registry v1 data from `b9c13b9` opens under the current build, and its grant still authenticates.</li></ul>Not covered: interrupting an install part-way, and signed release artifacts. |
| WM-702 | Done (local) | `uninstall` stops the daemon, removes the autostart entry and removes binaries; client entries go through `disconnect`. |
| WM-703 | Done | `docs/operations.md`, `docs/runbooks.md` |
| WM-704 | Partial | `release.yml` builds a draft, unsigned release with `SHA256SUMS` on tag. **Code signing and provenance are NOT RUN.** The hosted release workflow has not run. |
| WM-705 / 706 | **NOT RUN** | Demonstration with a downloaded binary; 1.0 evidence matrix. |

## M8

Not started: GUI, other platforms (Unix sockets exist only on the client side), WrongIndex adapter, native embeddings.

## Performance (WM-010 / WM-604 / WM-605, synthetic)

Command:

```powershell
cargo build --release -p wrongmemoryd -p wrongmemory-bench
target/release/wrongmemory-bench --corpus s --runs 5 --calls 200 --out benchmarks/results/<date>-s.json
```

- Setup: a real daemon over a real named pipe, with a deterministic seeded corpus.
- Ten percent of the memories are session-private to another session and serve as hard negatives.
- Latencies are measured on the client side, so they include IPC.
- The machine is a local Windows 11 developer workstation, not dedicated benchmark hardware.

| Corpus | Load | RSS after load | search p50 / p95 / p99 | forPath p50 / p95 / p99 | propose p50 / p95 / p99 | recall@5 | leaks |
|---|---|---|---|---|---|---|---|
| S (1,000) | 4.0 s | 16.4 MiB | 0.38 / 0.70 / 0.94 ms | 0.45 / 0.77 / 1.11 ms | 0.29 / 0.45 / 0.85 ms | 1.00 (300 queries) | 0 |
| M (25,000), before the path fix | 1,085 s (inherited, see below) | 18.7 MiB | 2.27 / 4.66 / 5.84 ms | 42.3 / 53.8 / 60.9 ms | 0.40 / 0.93 / 1.28 ms | 1.00 (300 queries) | 0 |
| M (25,000), after the path fix (`benchmarks/results/2026-11-01-m.json`) | 954 s (inherited, see below) | 18.8 MiB | 1.22 / 2.78 / 4.02 ms | 2.87 / 5.13 / 7.31 ms | 0.24 / 0.45 / 1.36 ms | 1.00 (300 queries) | 0 |

The recall figure comes from a synthetic corpus of code tokens. It is not a quality claim for real tasks.

Findings from the M run:

- **Fixed: the near-duplicate importance window sorted the whole scope.** That made each `remember` O(n), at 6 writes/s at 10k. It is now one ordered index walk per status, merged on the same key, with the same rows and order.
- **Fixed: path lookup was O(n).** There were two causes:
  - The plan walked every memory in importance order and probed `id IN (edges…)` for each row.
  - The symbol arm used the `from_node` range implied by `GLOB 'mem:*'`.

  Now the hit ids are materialized with `idx_edge_to` pinned on every arm (an exact BINARY prefix range replaces the GLOB) and joined to `memories` by primary key. The result is the same rows; the path oracle passes, including symbol anchors.
  - Reproduction on a 25k-row copy: 9.2 ms → 0.013 ms.
  - A first attempt that only split the GLOB did not help (45 ms in the second M run).
  - Re-measured M: 45 ms → 2.87 ms p50 (other builds were running on the machine during the load phase).
- **Inherited baseline cost, kept for parity: the lexical near-duplicate window.** It is `bm25` over an OR of every prefix term with `LIMIT 32`, as in `sqlite-store-remember.ts` at `222d23a`.
  - It scores every row that shares any term, so `remember` slows as lexically overlapping rows grow.
  - About 60 ms per write at 16k in this corpus, where every row shares common words.
  - Changing it would change which rows dedupe considers.

## Verification run (2026-11-01, Windows 11, local)

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo test --workspace --locked` ×5 | PASS 140/140 on each run; no leftover daemon processes |
| `bun run test:client` (TS client 10/10, contract parity) | PASS |
| Adapter: wrongmemory-client 6/6, sage port 20/20 against the release daemon | PASS |
| Hosted CI (`32f0e8b`) | PASS |
| Real MCP clients, live store adoption, signed release | NOT RUN |
