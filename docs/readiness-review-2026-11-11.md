# WrongMemory readiness review - 2026-11-11

## Verdict and evidence boundary

WrongMemory has a working native service and substantial compatibility coverage. It is a preview with measurable strengths, not a zero-defect guarantee or a certified native 1.0. The largest remaining work is acceptance evidence, sustained-load behavior, retrieval-quality enforcement and installed-client coverage. Replacing the architecture or adding a vector database is not justified by the evidence collected here.

Reviewed WrongMemory source: `7ffce0bb7c243ecdf7354fe062b70f90a232fd05`, workspace 0.1.12, plus the bounded-logging patch described below. Reviewed WrongStack source: `a6211d479`, package version 1.5.6. WrongStack's checkout contains unrelated changes, but `packages/sage` and `packages/wrongmemory-client` are clean. They were read and their two real-daemon integration suites were run; no WrongStack source was edited.

This review inspected the binding plan, current contracts, daemon routing/authentication, project lanes, scheduler, semantic cache, MCP/configuration paths, operations, CI and recorded results. It is not exhaustive line-by-line coverage or an independent security certification. No live user `sage.db` was opened, adopted or changed. No client configuration, installed daemon, release or global setup was changed.

GitHub was queried on 2026-11-11:

- [WrongMemory v0.1.12](https://github.com/WrongStack/wrongmemory/releases/tag/v0.1.12), published 2026-11-07, has three platform archives, six executables and `SHA256SUMS`.
- [WrongStack v1.5.6](https://github.com/WrongStack/WrongStack/releases/tag/v1.5.6), published 2026-11-10, supersedes the old 1.4.0 status. Local ancestry confirms integration merge `784b0cd` is in tag `v1.5.6` (`284007538ccdd302ad2e9c0a0329383283bc473f`).
- [WrongMemory CI run 42535063036](https://github.com/WrongStack/wrongmemory/actions/runs/42535063036) is green on the reviewed committed HEAD. It does not certify this session's uncommitted logging patch.

The published assets were not downloaded or attestation-verified again in this session. Earlier downloaded-artifact and client runs remain dated evidence, not fresh certification of this patch.

## What is already substantive

| Area | Current evidence | Limit |
|---|---|---|
| Native core | Seven Rust workspace crates; no required JS runtime in the installed daemon/CLI | Bun remains a development/oracle/dashboard dependency |
| Storage | SAGE-profile DDL, raw-JSON patching, oracle read/mutation/lifecycle replay, read-only inspector | Parity is against recorded WrongStack baseline `222d23a`, not every future host change |
| Ownership | OS-backed guard, adoption marker, pre-write backup, handoff/export and restart tests | Existing uncooperative legacy binaries are outside the cooperative protocol |
| Isolation | Project/session/candidate/graph/vector/count/event tests; hidden IDs follow not-found semantics | Passing fixture cases do not exhaust every possible input/interleaving |
| Recovery | Receipts, revision conflicts, crash-before/after-commit, disk-full and interrupted-install tests | Not every operation uses durable replay receipts; natural no-op behavior is a separate contract |
| WrongStack | A full host adapter exists in WrongStack; 25 real-daemon adapter/worktree tests passed here | Not a fresh downloaded v1.5.6 host demonstration |
| MCP | Official rmcp 3.8.0, stdio bridge, optional authenticated loopback HTTP; current and legacy HTTP fixtures | Real named-client runs were not repeated here |
| Quality | Synthetic controls and a held-out task corpus exist | The held-out test currently gates leakage, not a minimum recall score |

## Integration contract to ship

### WrongStack native integration

```text
WrongStack MemoryPort / SAGE capabilities
  -> WrongMemoryPort (host adapter)
  -> authenticated private IPC
  -> wrongmemoryd
  -> one authoritative project sage.db
```

The host owns tool/turn rendering, context budgets, injection tracking, active-session context and optional capture hooks. The daemon owns storage, grants, visibility, revisions, mutations and durable maintenance. A timeout must never activate an inline SQLite writer. Use the same MemoryStore/MemoryPort object and one store owner.

The existing host vector wrapper asks for lexical results and owns its fusion. External MCP search requests daemon `semantic=auto`. Preserve exactly one fusion owner per request; mirror events must include changes from external clients and exclude duplicate handling of the host's own committed events.

The adapter bundled in this repository's `packages/client-ts/src/port.ts` is explicitly a **skeleton**. The production host adapter is in WrongStack's `packages/sage/src/wrongmemory-*`. Do not direct consumers to the skeleton as the complete integration.

### Independent coding tools

```text
Coding client -> wrongmemory mcp --project ID --grant ID
             -> the same authenticated daemon -> the same project store
```

Use a fixed project and a read-propose grant by default. Keep credentials in the private data root. Client configuration names a project/grant, not a host token or grant secret. EOF ends the bridge; the detached daemon remains available to other clients. WrongStack need not be running.

Generate the actual entry instead of copying the old plan's illustrative `--profile` examples:

```text
wrongmemory connect print --project <project-id> --grant <grant-id> --format json
wrongmemory connect print --project <project-id> --grant <grant-id> --format toml
```

MCP transport does not force recall, capture private conversations or guarantee task success. Publish two separate support columns: transport/policy conformance and observed agent workflow. Evaluate task-start recall, file-specific recall, proposal submission and stale/conflicting evidence behavior separately.

Optional HTTP currently runs as the separate `wrongmemory http serve` CLI listener, forwarding to the same daemon. It is not an endpoint inside `wrongmemoryd`, as the original plan proposed. It binds loopback, rejects browser Origins, validates Host, authenticates each request and applies limits. Document that additional process; either accept the topology with a recorded decision or implement daemon hosting as separate work. Do not expose it remotely as part of local setup.

### Client coverage

| Client | Repository's recorded evidence | This session | Next requirement |
|---|---|---|---|
| Claude Code | Windows stdio and HTTP runs; versions recorded in WM-705/WM-308 | NOT RUN as an installed client | Repeat current-version discovery, recall/propose, denied-write, shutdown and reconnect |
| Codex CLI | Windows stdio and HTTP; guided/unguided task experiments | NOT RUN as an installed client | Repeat with current approval behavior; distinguish CLI from IDE/app certification |
| Antigravity | Recorded installed-client stdio run | NOT RUN | Verify current configuration location/version and actual task use |
| MCP Inspector | Recorded stdio/HTTP runs | NOT RUN as Inspector; Rust protocol fixture suites PASS | Repeat against downloaded release |
| Cursor | Preset exists; prior matrix explicitly disclaims certification | NOT RUN / named certification UNSUPPORTED | Required by plan section 4; do not treat it as optional without an explicit plan/ADR change |
| VS Code/Copilot | No named WrongMemory preset/certification | NOT RUN | Adapt configuration to the actual harness and test it |
| Gemini CLI, Cline, Zed | Generic bridge is the candidate integration path | NOT RUN | Add owned-format presets only after official-format checks and installed-client tests |

Current official documentation confirms stdio/HTTP support in [Codex](https://learn.chatgpt.com/docs/extend/mcp), [Claude Code](https://code.claude.com/docs/en/mcp), [Cursor](https://cursor.com/docs/mcp), [VS Code](https://code.visualstudio.com/docs/agent-customization/mcp-servers), [Gemini CLI](https://geminicli.com/docs/tools/mcp-server/), [Cline](https://github.com/cline/cline/blob/main/docs/mcp/mcp-overview.mdx) and [Zed](https://zed.dev/docs/ai/mcp). This establishes a documented integration path, not a WrongMemory runtime pass.

Client-specific points found during the refresh:

- Codex reads initialization `instructions`; its official guidance asks for the first 512 characters to stand on their own. WrongMemory already puts recall/evidence guidance first. Keep proposal approval optional and scope it to proposal tools.
- Claude Code documents Windows stdin-close followed by process-tree termination after 300 ms. Retain the detached-daemon/EOF tests and add a real current-client shutdown probe.
- Cursor's current stdio documentation marks `type: "stdio"` required. The current generic JSON entry emits `command` and `args` only (`connect.rs::json_entry`). This is a documentation/configuration mismatch needing an actual client probe, not a demonstrated Cursor failure.
- VS Code/Copilot has harness-dependent configuration destinations. Zed uses `context_servers`. Do not reuse an `mcpServers` file blindly for every tool.
- Local stdio/loopback does not grant a cloud worker access to this computer. Remote deployment/authentication is a separate product scope.

Protocol claims must remain revision-specific. The [2026-07-28 HTTP binding](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http) requires per-request protocol/header metadata and request-scoped cancellation. The repository has both 2025-06-18 and 2026-07-28 HTTP fixtures; SDK compilation alone would not establish that.

## Fresh performance evidence

Windows x64 development machine, 32 logical CPUs; optimized locally built daemon, disposable stores, real IPC. This is not the plan's controlled 8-CPU reference-machine certification. Other development activity was present; no before/after speedup is claimed.

`wrongmemory-bench --corpus s --runs 5 --calls 200` after the logging patch:

| Operation | Samples | p50 | p95 | p99 |
|---|---:|---:|---:|---:|
| Search | 1,000 | 0.368 ms | 0.654 ms | 0.791 ms |
| Path lookup | 1,000 | 0.288 ms | 0.412 ms | 0.617 ms |
| Propose | 250 | 0.320 ms | 0.503 ms | 1.445 ms |

Corpus: 1,000 memories, 100 session-private hard negatives; 300 synthetic queries, recall@5 1.0, session leaks 0. End RSS 17,866,752 bytes, approximately 17 MiB. Full distributions: [2026-11-11-s.json](../benchmarks/results/2026-11-11-s.json).

Short mixed-load smoke on the final local optimized daemon: 40 projects, four clients, 130.31 seconds at a target 100 operations/s; **12,959 operations, zero errors**, achieved 99.44 operations/s. Search p95 **11.01 ms**, p99 **17.94 ms**; remember p95 **13.61 ms**, p99 **22.20 ms**. The largest observed search call was 354.53 ms, so a small p95 does not imply every call met that latency. There were 24 successful maintenance passes and 16 queued jobs; this does not prove all projects completed maintenance. The approximately 62-second post-load observation used 0.884% of one CPU core, ended at 44,568,576 bytes RSS (42.5 MiB) and 32 open canonical stores. Maintenance was still active, so this is not the plan's five-minute idle test. [Full report](../benchmarks/results/2026-11-11-soak-smoke.json), [samples](../benchmarks/results/2026-11-11-soak-smoke.samples.jsonl), [source/binary manifest](../benchmarks/results/2026-11-11-review-manifest.json). Neither 130 seconds nor three samples establishes a long-term resource plateau.

The historical 25,000-row benchmark reports search p95 2.78 ms and p99 4.02 ms. It is a different source/workload, not a fresh medium-corpus result. The November 7 soak report completed 24 hours of wall time with about 17.4 hours of active load, 8,639,998 operations and zero errors, including maintenance. Its sleep gap and catch-up burst prevent a continuous-24-hour-load claim. Recorded mixed-load p99 values were approximately search 183 ms, remember 515 ms and listPage 574 ms; these cannot be substituted for the short warm-lexical workload or compared as an equivalent benchmark.

## Confirmed defect fixed in this session

**F01 / WM-604: log rotation only ran at startup.** A running daemon appended indefinitely despite declaring a 5 MiB file quota. A small event was the unaffected control; 340 synthetic events then produced **11,171,116 bytes**, exceeding the expected **5,242,880**. The proof failed on that assertion before production code changed.

`crates/wrongmemoryd/src/log.rs` now checks UTF-8 byte size before each file append, rotates under its existing mutex, closes the Windows file handle before rename and replaces only the single backup. Whole JSON records are retained; an individual event exceeding the quota remains on stderr. The normal write path uses an in-memory byte counter, with metadata refresh only after a write error. No dependency, storage or IPC contract changed.

Three permanent tests cover runtime rotation through the real logger, exact byte boundary/Unicode/oversized-event behavior and startup with an existing backup. The proof and independent three-run verifier are retained under `.temp_files/`; their ledger is `ledger_system-readiness.md`. This patch is local and is not in the published v0.1.12 binaries.

## Remaining work, in order

These are acceptance tasks or source observations, not additional reproduced bugs.

| Priority / review ID | Existing task | Required work | Exit evidence |
|---|---|---|---|
| P0 / R1 | WM-706 | Keep the current evidence matrix and status aligned with actual releases | No stale "latest 1.4.0" or "soak still running" claims; each row has source/artifact/date/status |
| P0 / R2 | WM-705, WM-402..409 | Repeat the 12-step demonstration with downloaded WrongMemory 0.1.12 and WrongStack 1.5.6 | Checksums/attestations, worktree evidence, injection/capture/review, vector mirror, one owner, restart and rollback on disposable adopted fixtures |
| P0 / R3 | WM-306, WM-309 | Close Cursor's required installed-client gap; refresh existing named clients | OS/client/version/revision/transport table, wrong-project/private-ID denied, revoke/reconnect/EOF, actual recall and proposal behavior |
| P0 / R4 | WM-604 | Continuous 24-hour active mixed load on the final patched artifact; include maintenance and resource trends | No sleep gap/catch-up burst; zero errors/leaks, handle/thread/cache plateaus; queue and tail-latency distributions; no live user workload |
| P1 / R5 | WM-604, WM-205 | Measure M/L corpora and noisy-neighbor latency during maintenance | Five comparable runs; M warm lexical p95 <=50 ms/p99 <=150 ms; path p95 <=100 ms; noisy-neighbor p95 <=200 ms, as proposed by the binding plan |
| P1 / R6 | WM-605 | Make held-out recall a failing CI gate, preserving hard-negative zero tolerance | Fixed evaluation method and independently reviewed labels; fail an injected recall regression. The current held-out test prints metrics and asserts only `leaks.is_empty()`; the separate handwritten corpus already has thresholds |
| P1 / R7 | WM-604, WM-508/509 | Measure/bound semantic-cache and receipt retention | `SemanticCaches.open` is a per-project map without the canonical store pool's 32-handle eviction. Receipts have no observed pruning path. Measure many semantic projects and long-running keyed writes; define retry horizon before any receipt GC |
| P1 / R8 | WM-602 | Finish retry/deadline failure-path proofs | Entropy failure in `Bridge::call_once` currently falls back to the unkeyed retry path; prove/reject duplicate effects before changing it. Verify per-operation natural idempotency separately from replaying the original response; test stalled peer/deadline recovery |
| P1 / R9 | WM-704 | Close build/evidence automation gaps | CI currently runs Rust, TS tests/parity and licenses; add TS/dashboard typecheck, reproducible embedded-dashboard drift detection, held-out thresholds and benchmark artifact identity to a release gate |
| P2 / R10 | WM-801, WM-805 | Expand clients and optional neural retrieval only after the above | Client-specific probes, provider/model/license/resource/quality results; lexical fallback stays independent |

Maintenance currently holds the project's store mutex for the whole hygiene/doctor pass. It avoids starting when interactive work is already waiting, but does not preempt a pass after it starts. This is a source-grounded contention hypothesis to measure for R5, not proof that every interactive call is slow.

The built-in semantic provider is deterministic `ngram-hash`, not a neural multilingual embedding model. On the recorded held-out set, lexical recall@10 was 0.917 and hybrid recall@5 0.872. Strong subword matching does not establish meaning-level cross-language retrieval. The recorded precision@5 around 0.21 measures the annotated required-set definition; it is not proof that every other returned record is irrelevant. Improve datasets and human review before using that number as a product error rate.

Add neural inference only if blinded held-out task outcomes improve enough to justify cold-start, CPU/RSS, privacy and maintenance costs. Preserve one fusion owner and version provider/model/tokenizer/cache provenance. Signing, a tray app and global cross-project memory are optional increments; none substitutes for the required gates above.

## Session verification

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo test --workspace --locked` | PASS: 199 passed, 2 ignored opt-in store-copy probes |
| Logger verifier x3 and unchanged original proof | PASS: 3/3 logger tests on every run; original proof `PROBLEM NOT REPRODUCED` |
| `bun run test:client` | PASS: 10 tests; 55 methods / 23 error codes |
| `bun run typecheck`; dashboard `typecheck` | PASS |
| Dashboard Vite build to `target/review-dashboard-2026-11-11` | PASS; embedded committed assets were not overwritten |
| `cargo build --release --locked -p wrongmemoryd -p wrongmemory-bench` | PASS |
| WrongStack real-daemon adapter + worktree suites | PASS: 2 files, 25 tests, no skips reported |
| Five-run small benchmark | PASS for this workload; not M/L/24-hour certification |
| Short mixed-load smoke | PASS: 12,959 operations, 0 errors; 24 maintenance passes / 16 queued; not a continuous 24-hour result |
| `git diff --check` | PASS |
| Local Markdown links and benchmark source manifest | PASS |
| Fresh named-client / downloaded-artifact / physical Unix-client / continuous 24-hour proof | NOT RUN |

An intermediate overlapping Cargo invocation failed to replace a Windows debug executable while fixture daemons were using it. This was a build-file sharing error, not a regression assertion. Final focused verification is serialized after the full workspace run. The logging patch does not change IPC, bridge or process-lifecycle behavior, so the five-consecutive-run requirement for those changes is not invoked.

The initial `.gitignore` modification and untracked `.wrongstack/` were preserved. There was no commit, push, deployment or live-store handoff. Security observations beyond the existing disposable-fixture suites remain source review; no exhaustive or independent penetration-test pass is claimed.
