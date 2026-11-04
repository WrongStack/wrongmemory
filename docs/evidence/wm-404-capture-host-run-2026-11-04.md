# WM-404: outcome and error capture in a released WrongStack host, 2026-11-04

WrongStack's opt-in capture hooks (`Sage.capture.toolOutcomes`, `Sage.capture.errorPatterns`) had never run in a host session against WrongMemory. This run drove released WrongStack host sessions in `wrongmemory` mode, and the same sessions in `local` mode, then compared the two stores record by record. It found one deviation, the maintenance schedule, which is fixed in 0.1.8.

All data is synthetic. `WRONGSTACK_HOME`, `WRONGMEMORY_HOME`, `LOCALAPPDATA` (install prefix) and `WRONGMEMORY_CONFIG_HOME_OVERRIDE` pointed into a scratch directory, and `WRONGMEMORY_AUTOSTART_KEY_OVERRIDE` kept autostart out of the real Run key. The user's WrongStack home, WrongMemory data root and daemon were not touched.

## Setup

| Part | Identity |
|---|---|
| WrongStack host | released `wstack-windows-x64.exe` 1.4.0 (SHA-256 `ceabc082…2362e`, the asset verified in WM-705). Its `outcome-capture.ts` and `host-wiring.ts` match the adapter branch. |
| WrongMemory | the v0.1.7 draft package `wrongmemory-0.1.7-windows-x64.zip` (checksums and attestation verified), installed with `wstack sage service install --from <package>\bin`; later the 0.1.8 fix built from the working tree, installed with `--from target/release --stop-daemon` |
| Provider | a scripted OpenAI-compatible endpoint (`wstack --provider ollama --model fake`): the prompt `CALL <tool> <json>` becomes that tool call, so no model is involved |
| Config | profile `Sage.capture = {toolOutcomes: true, errorPatterns: true}`; one profile in `wrongmemory` mode, a second, otherwise identical profile in `local` mode |
| Projects | fresh git repositories with the same two files, one per mode |

Each step below is one `wstack` process with one tool call, so every step is a new host session.

## Capture in `wrongmemory` mode

| # | Session | Result |
|---|---|---|
| 1 | `bash` `git log --oneline -1` | PASS. One `tool_outcome` record through the daemon: scope `project`, importance 0.45, confidence 0.7, tags `auto-capture`, `tool_outcome`, `bash`, a `command` anchor, and one `tool_result` source with the session id and tool-use id. The daemon attributed it to principal `host`, client `wrongstack`. |
| 2 | `read` `src/missing-file.ts` | PASS. One `error_pattern` record: importance 0.55, confidence 0.6, the `FS_READ_FAILED` text, no anchor (the tool was given no command). |
| 3 | step 1 again, new session | PASS. No second record: the existing record merged it (`memory-merged`, revision 2, two sources from two sessions). |
| 4 | step 2 again, new session | PASS. Merged into the step-2 record, revision 2. |
| 5 | `bash` `git rev-parse --abbrev-ref HEAD` | PASS. A second `tool_outcome` record. |

An earlier session in another project also showed near-duplicate merging across tools: a `bash` and a later `read` argument-parse refusal, whose texts differ only in the tool name, became one `error_pattern` record with both sources and the tags of both tools.

## Comparison with `local` mode

A script read both stores read-only and compared every record after normalising ids, timestamps, session ids, tool-use ids, process ids and commit hashes.

| Check | Before the fix | After the fix |
|---|---|---|
| Records | 3 and 3: same kinds, texts, revisions, sources, anchors, tags, importance and confidence | same |
| Fields that differ | `wrongmemory` (the daemon's attribution, added by design) and `lastVerifiedAt` on the two `tool_outcome` records | `wrongmemory` only |
| Audit trail | local: `memory.hygiene_dedup`, `memory.hygiene_completed`; WrongMemory: none | both: `memory.hygiene_dedup`, `memory.hygiene_completed` with the same report (examined 3, verified 3) |

## Finding: the maintenance schedule (fixed in 0.1.8)

The `lastVerifiedAt` difference came from maintenance. A host registers its hygiene options at boot (`surface.maintenance('schedule')`). The baseline project server (`project-server-hygiene.ts`) then runs a store's first pass 90 s later (`MAINTENANCE_SETTLE_MS`) when none ran in the last 6 hours, and every 6 hours after that (`MAINTENANCE_INTERVAL_MS`). The local store had its pass 97 s after the server started.

WrongMemory gave each project a schedule whose first run came 24 hours after registration and then once a day. Host teardown hygiene reached neither store in these sessions, as the baseline's own comment describes. So in `wrongmemory` mode a new or adopted store went its first day without hygiene, and afterwards ran it a quarter as often as the baseline. That delays session expiry and anchor verification.

The fix:

- A new schedule's first run is 90 s after it is created, then every 6 hours.
- A one-time step when the daemon opens its registry moves schedules still on the old 24-hour default to 6 hours. Their next run is due at most 6 hours after the last run, or 90 s after start if none ran.
  - The step adds no schema version, so a 0.1.7 daemon restored from `.old` still opens the registry.
  - A cadence set later through `maintenance.configure` is left alone.

Re-run on the fixed build, same scratch data root:

| Check | Result |
|---|---|
| Daemon restart with two projects registered under 0.1.7 (24-hour schedules that had never run) | PASS: their first passes ran 90 s and 95 s after the daemon was ready (one scheduler tick apart) |
| A project registered on the fixed build | PASS: first pass 100 s after registration |
| Five-step scenario in both modes, compared after both passes | PASS: identical apart from the `wrongmemory` attribution |

## Not covered

- Teardown hygiene: neither mode's store recorded a pass from host teardown in these one-shot sessions, so it is not compared.
- The domain-term mirror and the session-end commit extractor, which write only `.wrongstack/domain-terms.md`, were not inspected.
- Path remap through the daemon was run in the WM-705 addendum, not here.
