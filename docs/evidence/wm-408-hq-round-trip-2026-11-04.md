# WM-408: live HQ round trip through WrongMemory (2026-11-04)

This run synced SAGE memory between two checkouts of one project through a real WrongStack HQ server. Each checkout's store was owned by WrongMemory. It used released binaries in a disposable environment on Windows 11.

`WRONGSTACK_HOME`, `WRONGMEMORY_HOME`, `LOCALAPPDATA`, `WRONGMEMORY_CONFIG_HOME_OVERRIDE` and `WRONGSTACK_HQ_DATA_DIR` pointed into a scratch directory. The owner's WrongStack home, HQ data, WrongMemory data root and daemon were not touched; the live daemon kept running throughout. All data is synthetic. The HQ server printed its first-run client token to its own log in the scratch directory; it is not reproduced here.

## Artifacts

| Artifact | Identity | Checks |
|---|---|---|
| WrongStack `wstack-windows-x64.exe` | release v1.4.0. It contains the adapter commit `fe546f2ab`, which routes HQ SAGE sync through the daemon | SHA-256 `ceabc082…2362e` matches the release `SHA256SUMS` |
| WrongMemory `wrongmemory.exe`, `wrongmemoryd.exe` | release v0.1.6, from the verified zip | `SHA256SUMS` OK |

## Setup

1. `wstack sage service install --from <v0.1.6 package>\bin`, then `wstack sage service use wrongmemory --yes` in project A. This created `.wrongstack/project.json` (`proj_01M6JQRHE81N1DG9ZRF0847GZ8`), which was committed.
2. Project B is a `git clone` of A, as a second machine would have it. It has the same HQ project identity but its own directory and its own WrongMemory store.
3. HQ server: `wstack hq --host 127.0.0.1 --port 3519 --strict-port --data-dir <scratch>\hq`.
4. In A, then in B: `wstack sage sync`, the headless SAGE HQ bridge.
   - Each sync registered its project with WrongMemory as a `managed` store: A as `p_04ee97b8f5184e46` and B as `p_15e91d507eba49d0`. The daemon was started on demand (0.1.6).
   - Neither checkout got a `.wrongstack/memories/sage.db` or a `server.json`.
   - No SAGE project server process ran. The processes were one `wrongmemoryd`, the HQ server and the two sync bridges.

## Steps

| # | Step | Result |
|---|---|---|
| 1 | Write in A with `wrongmemory remember`: a project decision `…VV4` and a warning `…PCK` anchored to `src/config.ts`. | PASS. HQ's SAGE store for the project held both at revision 1 within seconds. |
| 2 | Start B's sync after A had pushed, as a late joiner. | PASS. `wrongmemory search` on B returned both records with the same ids and revision 1. |
| 3 | Write in B through MCP with a curator grant: fact `…ZD`, anchored to `src/config.ts`. | PASS. It appeared in A, and A's `for-path src/config.ts` returned it through the path channel, so anchors survive the trip. |
| 4 | Update the decision in A through MCP (`memory_update`, `expectedRevision: 1`). | PASS. B shows revision 2 with the new text. |
| 5 | Delete the warning in A through MCP (`memory_delete`, `force: true`). | PASS. B shows revision 2 `deleted`, and B's search no longer returns it. |
| 6 | Check for echo. | PASS. HQ holds one entry per record at its writer's revision: rev 2, rev 2 deleted, rev 1. The imports did not come back as new changes. |
| 7 | Stop the HQ server, write `…17V` in A, then restart the HQ server. | PASS. B had the record about 5 seconds after the restart (checked by id). |

## Not covered here

- Session-scoped records are never projected to HQ. That is covered by `hq_sync_is_host_only_and_never_projects_sessions`, not by this run. The CLI has no session-scoped `remember`.
- Concurrent conflicting edits in both checkouts. Ordering and conflicts are covered by the oracle scenario `hq-sync` (`hq_sync_scenario_matches_oracle`).
- An HQ server on another machine, and HQ with token authentication configured by hand. Same-machine discovery supplied the token here.
- `auto` mode: this run used `wrongmemory` mode only.
