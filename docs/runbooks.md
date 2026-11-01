# Runbooks

Each runbook starts with **read-only diagnosis**. Commands that change state are labelled **MUTATES**. "Delete sage.db and retry" is never a remedy: memories cannot be rebuilt from source.

Read-only commands used throughout:

- `wrongmemory doctor`
- `wrongmemory daemon status`
- `wrongmemory project list`
- `wrongmemory migrate inspect <store>`
- `wrongmemory grants list`
- `wrongmemory client doctor <client> --project <id>`
- `wrongmemory diagnostics export --output <dir>`

## Daemon startup failure

1. Diagnose: `wrongmemory daemon status` (exit 3 means not running). Read the last lines of `<data-root>\logs\wrongmemoryd.log`. Run `wrongmemoryd --data-root <root>` in a console to see the error directly.
2. Interpret the result:
   - Exit 3, "another daemon already owns": a daemon is already running for this data root, which is fine.
   - "held by a process that is not this user's daemon": another account squats the pipe name. The daemon refuses on purpose; use a different `--data-root`.
   - "registry schema … is newer": the binary is older than the data. Install the newer binary instead of downgrading.
3. Fix: **MUTATES** `wrongmemory daemon start`.

## Incompatible profile or client

- Diagnose: run `wrongmemory migrate inspect <store>`, then check the verdict.
- `older`: open the project once with its WrongStack version so SAGE migrates the store, then inspect again.
- `newer`: upgrade WrongMemory.
- `divergent` or `unknown-data`: do not adopt for writing.
- Protocol mismatch (`PROTOCOL_UNSUPPORTED`): the client and daemon versions differ. Install matching versions.

## Owner conflict

- Diagnose: `wrongmemory project show <id>` shows the admission reason and owner epoch. Check whether `<store-dir>\wrongmemory-owner.lock` is held by another daemon or data root, and whether `server.json` exists (legacy SAGE daemon).
- WrongMemory never takes over a store whose owner might still be writing.
- Fix:
  1. Stop the other owner: WrongStack's SAGE daemon, or the other data root's daemon.
  2. **MUTATES** `wrongmemory project add <root> --write --force-owner`.

## Duplicate store discovery

- Diagnose: `wrongmemory project add` returns `STORE_IDENTITY_CONFLICT` when the root already has a registered store, or when a managed store would shadow an existing `.wrongstack\memories\sage.db`.
- Fix: adopt the existing store; never create a second authoritative store. Merging two stores is a reviewed migration and is not automated in this release.

## Revoked credential

- Diagnose: `wrongmemory grants list` shows `revokedAtMs`, and `wrongmemory client doctor <client> --project <id>` reports the grant as inactive. Bridges report `UNAUTHENTICATED`.
- Fix:
  1. **MUTATES** `wrongmemory connect <client> --project <id> --create-grant`, which writes a new grant id into the owned entry.
  2. Restart the client.

## Session-hidden memory

- Diagnose: session-scoped memories are visible only to their owning session. An external grant always runs in its own daemon-issued session. `NOT_FOUND` for a hidden id is expected and indistinguishable from an absent id.
- Nothing to fix. If the knowledge should be shared, propose it with `scope=project` and have a curator accept it.

## Missing worktree source

- Diagnose: `memory_hygiene` with `dryRun: true` (curator) reports `staled` counts. A missing anchor file becomes `stale`, with `staleReason: verification`. It is never deleted, and it reactivates when the file returns.
- If the project root is unreadable, verification is skipped entirely.
- Fix: none needed. **MUTATES** `memory_hygiene` without `dryRun` applies the verification.

## Corrupt memory row

- Diagnose: search and listing results show a `corruptSkipped` count. `doctor` gives per-store counts. Valid sibling rows keep working.
- Fix:
  1. Back up the store: **MUTATES** (a new file) `wrongmemory backup …`.
  2. Repair the row with SAGE's doctor from WrongStack on a copy.
- WrongMemory does not auto-delete corrupt rows.

## FTS repair

- Diagnose: `wrongmemory migrate inspect <store>` shows `ftsPresent`.
- A native FTS rebuild command is **not implemented** in this release. Without FTS5, lexical search uses the NFKC `LIKE` channel automatically.
- Fix: run WrongStack's `fts-rebuild` doctor fix on the store while the WrongMemory daemon is stopped.

## Semantic provider unavailable / stale vector mirror

- Not applicable in this release. Retrieval is lexical-only, plus a deterministic hashing rerank that needs no provider and has no network dependency.

## Stalled candidate

- Diagnose: `wrongmemory candidates list <project>`. The `memory_candidates` tool with `action: list` shows the same queue.
- Fix (curator, **MUTATES**): accept, reject, or resolve. Use the `memory_candidates` tool or the daemon methods `candidate.accept`, `candidate.reject`, `candidate.resolve`.
- A stale `targetRevision` returns `REVISION_CONFLICT`. The candidate stays pending, and a `memory.candidate_resolution_reverted` audit row is written.

## Revision conflict

- Diagnose: `REVISION_CONFLICT` (exit 6) means someone else changed the memory since you read it.
- Fix:
  1. Re-read it with `wrongmemory get <project> <id>`.
  2. Reassess the change.
  3. Retry with the new `expectedRevision`.
- Never retry blindly.

## Maintenance backlog

- Diagnose: `memory_hygiene` with `dryRun: true` reports the work that would run, without writing.
- Fix: **MUTATES** run `memory_hygiene` (curator). It runs in a single transaction per project.

## WAL growth

- Diagnose: check the size of `sage.db-wal` beside the store. Long-lived readers delay checkpoints.
- Fix:
  1. Stop idle bridges and clients.
  2. **MUTATES** `wrongmemory daemon stop`, then `daemon start`. Closing the store checkpoints the WAL.

## Disk full

- Diagnose: mutations fail with `DISK_FULL` (exit 1). The failed transaction is rolled back and the last committed knowledge is intact.
- Fix: free space, then retry with the **same idempotency key**. The durable receipt returns the original outcome if the commit actually happened.

## Interrupted migration or update

- Diagnose: run `wrongmemory migrate inspect <store>`, then `wrongmemory doctor`.
  - Store extension activation is one transaction: it either fully applied or not at all.
  - A binary update keeps `*.exe.old`.
- Fix: **MUTATES** rerun `wrongmemory install --stop-daemon`, or roll back the binaries (see operations.md).

## Backup and restore

- See operations.md.
- `backup` creates a new file and never overwrites one. Restoring is a manual, explicit file replacement with the daemon stopped. Keep the replaced files.

## Service release (hand a store back to WrongStack)

1. Diagnose: `wrongmemory project show <id>` shows the owner epoch.
2. **MUTATES** `wrongmemory project detach <id>`. It releases the owner guard (recorded in the registry) and keeps the store.
3. Start WrongStack in its local mode.
4. Managed stores (`<data-root>\projects\…`) have no WrongStack location. Export them with `wrongmemory backup` first.

## Disconnect a client

1. Diagnose: `wrongmemory client doctor <client> --project <id>`.
2. **MUTATES** `wrongmemory disconnect <client> --project <id> [--revoke] [--dry-run]`. It removes only the owned `wrongmemory` entry and guidance block.

## Uninstall

1. Diagnose: `wrongmemory autostart status`, then `wrongmemory project list`.
2. **MUTATES** `wrongmemory uninstall`. Data is kept.
3. **MUTATES, irreversible** `wrongmemory uninstall --remove-data --yes`. It deletes the data root, including managed stores. Back up first.
