# Native desktop program - GPUI feasibility and product contract

Date: 2026-11-11. Task: WM-800, with WM-801 and WM-604 acceptance dependencies.
State: optional native Windows preview published in [v0.2.1](https://github.com/WrongStack/wrongmemory/releases/tag/v0.2.1); [release evidence](../releases/v0.2.1.md) records hosted CI, downloaded artifacts and fixture install/update/uninstall results. GPUI Kit is the selected framework. Existing WM-INV-01..16 and WM-ADR-001..013 remain binding; [WM-ADR-014](adr-014-native-desktop-workspace.md) records the independent desktop workspace/toolchain. Product targets below remain acceptance requirements, not blanket completion claims.

## Product objective

Make WrongMemory a fast, keyboard-accessible knowledge workbench: find the right record, understand its applicability and provenance, review a proposed change and see service health without knowing CLI syntax. A desktop framework alone does not provide this product quality.

Implemented topology: an optional `wrongmemory-desktop` native Rust executable using GPUI, connected to the existing daemon over authenticated IPC. The browser dashboard remains available while native capability and installed-platform equivalence are established. Neither UI becomes a second SQLite owner. Closing either UI releases its own connections and leaves the service and MCP clients running.

## Current product and specific opportunities

The browser app already has project overview, memory search/list, detail/edit/recovery, review, React Flow graph, activity, health/maintenance and read-only grant/service views. Source: `packages/dashboard/src`, `crates/wrongmemory-cli/src/ui.rs`.

The new desktop should add coherent workflows rather than simply reproduce those tabs:

| Surface | Product requirement | First-delivery boundary |
|---|---|---|
| Workspace shell | Resizable project navigation, central working view, persistent inspector, command palette, shortcuts and saved layouts | One project per working tab; no implicit global memory namespace |
| Memory library | Virtualized rows, query/filter chips, kind/status/anchor filters, saved local views, keyboard selection and optional compact/card presentation | Server pagination and limits; no fetch-all on startup |
| Evidence inspector | Full text, provenance, anchors, current revision, applicability outcomes and checkout labels; clear stale/unknown states | Show existing evidence, not a fabricated truth score or unrestricted source-file reader |
| Review workbench | Current target beside proposed revision; evidence and conflict reason visible before accept/reject | Revalidate target revision; partial batch outcomes reported per candidate |
| Relations | Pan/zoom, focus selected memory or path, relation legend, explicit depth/limit/truncation and inspector linkage | Bounded daemon-returned subgraph; no thousands of offscreen widgets or automatic whole-store traversal |
| Clients | Project/profile/grant state, connect plan preview, doctor, disconnect/revoke with explicit consequences | Never render secrets; client/version support labels follow actual certification |
| Health | Service identity/version, owner/store profile, jobs, queue state, repair preview, typed failures and reconnect state | Starting/stopping the service is an explicit operator action, separate from window close |

The current web app refreshes per-project statistics for all projects every 20 seconds. The native shell should load visible/active projects under a bounded concurrency budget, cache summaries and invalidate from changes. It should not repeatedly open every registered store merely to repaint a sidebar.

Complete immutable text history is not currently an established daemon capability. An audit trail is bounded, and a revision number does not reconstruct old record text. Deliver current-target/proposal comparison first; a true historical diff browser requires separately designed persistence and IPC support. Similarly, do not advertise a universal Undo: recovery or a compensating edit has its own revision and authorization semantics.

## Framework findings, verified 2026-11-11

| Candidate/source | Finding | Consequence |
|---|---|---|
| [Official GPUI](https://gpui.rs/) and [upstream README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md) | GPU-accelerated Rust framework; pre-1.0, frequent breaking changes; standalone platform entry point described separately | Pin a coherent API/package set; no unpinned Git main |
| crates.io `gpui` | Published stable 0.2.2, Apache-2.0, published 2025-10-22 | The published crate is not evidence of API equivalence with current upstream README |
| Upstream Zed toolchain | Current main pins Rust 1.98.1; WrongMemory pins 1.93.0 | This is an upstream baseline, not proof of minimum Rust version. Compile the selected package on 1.93 before deciding on an upgrade |
| [GPUI Kit](https://gpui-kit.com/docs/getting-started/) | Current stable `gpui-kit` and `gpui-component` are 0.9.1, Apache-2.0; published 2026-11-09 | A component library can reduce input/table/docking/theme work, but it is a distinct dependency choice |
| crates.io GPUI Kit dependency metadata | Kit 0.9.1 selects exact `gpui-pre` / `gpui-pre-platform` 0.3.12 snapshots; optional GPUI Fast is another engine family | Do not combine Kit 0.9.1 with the unrelated old direct `gpui` 0.2.2 types |
| [Installation requirements](https://gpui-kit.com/docs/installation/) | Windows MSVC, Windows SDK and CMake; platform-specific native dependencies | Build and launch on each claimed platform; documentation support is not WrongMemory certification |

Primary registry endpoints: [gpui](https://crates.io/api/v1/crates/gpui), [gpui-kit](https://crates.io/api/v1/crates/gpui-kit), [gpui-component](https://crates.io/api/v1/crates/gpui-component), [Kit 0.9.1 dependencies](https://crates.io/api/v1/crates/gpui-kit/0.9.1/dependencies). The unaliased `gpui_platform` registry lookup returned 404; the current Kit packaging uses `gpui-pre-platform` instead. Do not copy a current-source dependency example and assume those exact registry names are available.

An isolated preflight workspace lives under `.temp_files/gpui-preflight/`. `cargo generate-lockfile --manifest-path .temp_files/gpui-preflight/Cargo.toml` passed with 801 packages selected for Rust 1.93 compatibility. This is dependency resolution only. The subsequent actual `cargo check --locked --manifest-path .temp_files/gpui-preflight/Cargo.toml --target-dir target/gpui-preflight` **FAILED**: `gpui-pre 0.3.12/src/profiler.rs:475,496` calls `std::hint::cold_path()`, reported as unstable (`E0658`) by Rust 1.93. Thus the selected current Kit does not compile on the project's pinned toolchain, despite successful resolver selection and absent declared minimum-version metadata.

The same isolated check **PASSED** using already-installed Rust 1.99.1: `cargo +1.99.1 check --locked --manifest-path .temp_files/gpui-preflight/Cargo.toml --target-dir target/gpui-preflight`, Windows MSVC, 4 minutes 20 seconds for this first newer-toolchain check. It typechecked the selected dependencies and original window/view code (sidebar and explicit disconnected preflight state). No dependency was added to the product workspace, and its Cargo.lock/toolchain were not changed. This still requires an explicit workspace/toolchain strategy and full core gates before integration. It does not prove executable linking, a visible window, platform compatibility or UI quality.

After the newer-toolchain result, choose explicitly between a globally certified core-toolchain upgrade and an independently versioned desktop workspace/toolchain with its own required CI gate. The latter needs a recorded workspace/dependency-pinning decision; it must not become an untested directory outside normal release checks. The older published GPUI is a separate compatibility candidate, not an automatic fallback with current Kit APIs.

Lead candidate for the spike is exact `gpui-kit = "=0.9.1"`, with its matching snapshot family and committed lockfile after admission. Avoid enabling GPUI Fast, WebView, speech, every syntax grammar or optional native shell integration until a concrete feature needs them. UI uses the library API; product code, interaction design and assets are original rather than copied from Zed's application implementation.

## Architecture and data flow

```text
wrongmemory-desktop
  GPUI views + UI entities + local presentation preferences
      -> typed desktop commands / explicit action policy
      -> bounded async request service
      -> existing wrongmemory-ipc / wrongmemory-contracts
      -> wrongmemoryd -> authoritative store + registry

WrongStack and MCP clients continue to use that same daemon.
```

Implemented source layout (view extraction can follow when needed):

```text
apps/desktop/                  independent Cargo workspace and lockfile
  src/main.rs
  src/app.rs                 window, project context, routing and connection state
  src/service.rs             bounded IPC tasks, commands, reconnect and event cursor
  src/preferences.rs         validated private presentation settings
  src/model.rs               projections, graph bounds, generation and draft guards
  src/app/graph_view.rs      custom GPU relations canvas
  tests/daemon.rs            mandatory disposable real-daemon integration
```

Keep SQLite out of desktop dependencies. Authenticate privately through the existing Rust IPC path. Never place host/grant credentials in process arguments, view state dumps, clipboard content, logs or settings. The native operator can have host authority, so wrap it in an explicit desktop operation list; do not expose an arbitrary method invoker.

UI rendering never waits synchronously on IPC, SQLite, verification or graph layout. Use a bounded Tokio-backed service thread with typed messages into GPUI. The existing CLI's activation code is private to a binary; reusing it cleanly may require a small shared client/activation module. Treat that extraction as a lifecycle change, with the required five consecutive daemon/CLI integration runs. Do not copy startup logic into a second implementation.

For each request carry project ID, view generation, query generation and request identity. Ignore late results after tab/project/query changes; a cancelled view does not mean an admitted mutation was cancelled. Reconnect reconciles receipts/unknown commit outcomes before offering retry. Generate one mutation key per user intent and retain it over transport retry.

Use the existing authorized change feed with a cursor and coalesced refreshes. A ring gap or daemon-instance change triggers a bounded resnapshot. Telemetry updates should not invalidate every content view. Unmounting a view disposes its tasks/subscriptions; repeated tab changes must not leak connections or tasks.

Cross-project search is not an implicit new daemon capability. Initially search the selected project. A future multi-project view must explicitly enumerate authorized projects, apply per-project visibility before limits, label each result and cap fan-out. Ordinary management views continue to exclude session-private records.

## Interaction and visual specification

Target a restrained, dense desktop application: project rail around 240 px, working list around 440 px where space permits, flexible evidence inspector, draggable separators and a minimum usable single-pane layout. Long prose should remain readable rather than squeezed into tiny cards. Numbers, status chips and icons need text equivalents.

Shared semantic tokens: background, surface, raised surface, border, main/muted text, accent, focus, warning, error, success and unknown. Light/dark/system themes use the same semantics. Separate content status (active/stale/deleted), evidence outcome (satisfied/not-satisfied/unknown), job state and connectivity; colour must not conflate them. Typography, spacing and density are consistent across screens.

Required actions: Ctrl+K palette, Ctrl+P project switcher, Ctrl+F query focus, arrow navigation, Enter inspect, Escape back/close and predictable Tab order. Shortcut scope must respect text editors. Review accept/reject and destructive changes are explicit actions; avoid a single-key accidental destructive shortcut. Native clipboard and selection must work for long Unicode text and paths.

Required states for every async screen: connecting, loading, empty, filtered-empty, read-only, permission denied, unsupported capability, partial result, retryable failure, revision conflict and committed-outcome-unknown. Preserve an edited draft on connection loss/conflict and show the current server revision before a new submit. Do not substitute a zero count for an unreachable project.

## Ordered delivery and acceptance

| Stage | Work | Exit evidence |
|---|---|---|
| D0 / WM-800-GPUI | Isolated exact-version compile, native input/list/window probe, platform and license report | Windows compile + visible launch, IME/clipboard, 100/150/200% scale, keyboard focus; record untested platforms. Resolve toolchain decision explicitly |
| D1 / WM-800-SHELL | Native shell, themes, project navigation, daemon state, bounded typed IPC, virtualized library and evidence inspector | Real disposable daemon fixture; data/revisions match CLI; closing UI leaves service/MCP alive; late-result/reconnect tests |
| D2 / WM-800-REVIEW | Current/proposed comparison, revision-safe edit, accept/reject, recover, partial batch handling | Two clients race same revision; one conflict; preserve draft and report per-item outcome; no hidden-session content |
| D3 / WM-800-OPS | Clients setup preview, health/jobs, repair preview, relations and saved local views | Existing web capabilities mapped; explicit unsupported labels where missing; no arbitrary SQL/shell/file tool; graph bounds and resync tested |
| D4 / WM-800-RELEASE | Accessibility, GPU/environment behavior, platform packaging and downloaded-artifact proof | Fresh install/launch/update/uninstall, native process-tree proof and supported OS matrix; core headless distribution still works independently |

Do not retire the web dashboard or call the native app complete at D0/D1. The initial milestone is a usable library and evidence inspector over real fixture data, not a static page with fake counts.

Proposed performance targets, not observed results: first window <=1.5 s cold and <=0.5 s warm on a declared reference machine; typing/selection p95 <=50 ms; scrolling/frame p95 <=16.7 ms on a 60 Hz display during a fixed workload. Test 25,000/100,000 registered records with bounded paging; UI element count and memory should follow the visible window/cache budget rather than total records. Run sustained tab/search/reconnect cycles and show a stable resource trend. Measure IPC wait, UI layout and GPU rendering separately.

Native acceptance must include Windows text input/IME, keyboard-only use, screen-reader semantics, high DPI, multi-monitor movement, long paths, long Markdown, reduced motion, device/sleep/resume behavior, daemon restart and unsupported GPU environments. A fast renderer is not proof of accessible controls or a safe mutation workflow. macOS/Linux support stays NOT RUN until their actual builds and launches pass.

## Historical isolated preflight boundary

- Source and official package/platform research: PASS.
- Rust 1.93 isolated dependency resolution: PASS; 801 packages.
- Isolated compilation on pinned Rust 1.93: FAIL, upstream `cold_path` E0658 in `gpui-pre 0.3.12`.
- Isolated dependency + original window-code `cargo check` on installed Rust 1.99.1: PASS on Windows MSVC; no product toolchain change.
- `cargo fmt --manifest-path .temp_files/gpui-preflight/Cargo.toml -- --check`: PASS.
- Plan links and `git diff --check`: PASS.
- Native application implementation, visible runtime, accessibility and desktop performance: NOT RUN at the preflight checkpoint; superseded for implementation and initial rendering below.
- Core toolchain/dependencies: unchanged.

Evidence logs: `target/gpui-preflight-resolution.txt`, `target/gpui-preflight-check.txt` (1.93 failure), `target/gpui-preflight-check-1.99.1.txt` (1.99.1 pass). The experiment is not a shipped UI or a measured UI-performance result. The proposed first product slice remains a daemon-connected virtualized library and evidence inspector, followed by revision-safe review.

The core readiness work in `docs/readiness-review-2026-11-11.md` remains necessary. The desktop can be developed as an optional client without making daemon/MCP reliability depend on a window being open.

## Implemented preview and remaining acceptance

Source is in `apps/desktop`; operator/build instructions are in its [README](../../apps/desktop/README.md). The preview implements resizable panels, virtualized paged library, text/status search, evidence inspector, revision-checked edits with protected drafts and explicit current-revision comparison, review decisions, recover/delete confirmation, bounded relations with zoom/pan, public grant management and secret-free generic MCP configuration, health/jobs and explicit CLI-backed service startup. A bounded IPC worker and active-project change feed keep database work out of GPUI rendering. Presentation settings preserve unknown JSON fields and reject malformed/newer versions.

The independent workspace pins Rust 1.99.1 and exact GPUI Kit 0.9.1; core contracts/IPC remain on Rust 1.93. CI now includes a Windows native gate and repeated real-daemon tests. Release automation has a separately gated Windows desktop bundle with CLI/daemon, manifest, hashes, dependency metadata and available license texts. Local results belong in `docs/status.md`; configuring a workflow does not mean hosted CI has passed.

Initial Windows library and relations rendering was observed against a disposable fixture in the earlier preview. Screen automation was stopped by the user. Subsequent settings/conflict controls are verified by code/tests only: their visual/keyboard behavior has not been rechecked. Native accessibility/IME/high-DPI, sustained frame/latency targets, GPU fallback, sleep/resume and installed lifecycle remain NOT RUN. Linux/macOS native builds remain NOT RUN. No live store was adopted and no installed client configuration changed.

Still outside this slice: full immutable text history, automatic text merge/undo, batch review, saved text queries and kind/anchor filter controls, project creation/adoption UI, repair preview, cross-project search and automatic named-client installation. Do not infer these from the broader product target table. Drafts are memory-only; private presentation preference writes are asynchronous and are not a draft-recovery system.
