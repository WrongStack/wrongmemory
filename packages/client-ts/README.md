# @wrongmemory/client

Bun/Node client for the WrongMemory daemon, plus a skeleton WrongStack `MemoryPort` adapter (WM-102 TS side, WM-400 client half). It has no runtime dependencies beyond Node built-ins.

## Client

```ts
import { WrongMemoryClient, METHODS } from '@wrongmemory/client';

const host = await WrongMemoryClient.connect({ start: true, daemonPath: 'C:/…/wrongmemoryd.exe' });
const hits = await host.call(METHODS.MEMORY_SEARCH, { query: 'bun scripts' }, { projectId });

// Scoped external principal: the secret is read from <data-root>/grants/<id>.secret
const agent = await WrongMemoryClient.connect({ grantId: 'g_…' });
```

**Transport:**
- Windows named pipe from `runtime/endpoint.json`.
- Newline-delimited JSON with a 4 MiB frame limit; an oversized line is rejected before it is fully buffered.
- An auth frame goes first, then requests and responses run one at a time, matched by `requestId`.

**Errors:**
- Daemon errors become `WrongMemoryError` with `code`, `message` and `retryable`.
- Failures that happen on the client side use `UNAVAILABLE`, `TRANSPORT` or `PROTOCOL`.

**Deadline:** the default is 5 s per call. When a call times out, the client closes the connection, because a late reply would arrive out of sequence. A timed-out mutation may still have been committed. Retry it with the same `idempotencyKey` to get the original outcome.

**Starting the daemon:** `start: true` launches `wrongmemoryd` detached, with stdio ignored and the window hidden. It waits up to 10 s for the daemon to become ready.

**Deviation from the Rust client:** this client does not verify that the process serving the pipe runs as the same user (the Rust client's anti-squatting check). The protections here are the pipe's access list and the per-user endpoint name.

## MemoryPort skeleton

`createWrongMemoryPort({ client, projectId })` mirrors the capability IDs from `packages/sage/src/memory-port.ts` at the baseline commit.

`getCapability` behavior:

| Capability | Returned |
|---|---|
| `wrongstack.memory.sage-service.v1` | The service object |
| `wrongstack.memory.retrieval.v1` | `undefined` |
| `wrongstack.memory.surface.v1` | `undefined` |

The retrieval and surface capabilities need daemon operations that do not exist yet. They are reported absent rather than partly working.

| SAGE method | Daemon method | Notes |
|---|---|---|
| `searchSage(query, opts)` | `memory.search` | `includeStatuses` maps to `statuses`. `sessionId` binds a host session through `session.acquire`. `includeAllSessions` is refused with `FORBIDDEN`. |
| `getSage(id)` | `memory.get` | `NOT_FOUND` returns `null`, as SAGE does. |
| `retrieveForPath(opts)` | `memory.forPath` | Anchor matching only, without graph expansion. Statuses other than active/stale return `NOT_IMPLEMENTED`. `includeAncestors: false` drops hits that matched only an ancestor directory. |
| `rememberSage(input)` | `memory.remember` | A `scope: 'session'` record is owned by the bound session; `ownerSessionId` binds it first. |
| `createCandidate(input)` | `candidate.propose` | |
| `listCandidates(includeResolved)` | `candidate.list` | Returns pending only, or all four statuses when `includeResolved` is true. |
| `initialize()` | `project.get` | |
| `health()` | `daemon.health` | |
| `dispose()` | closes the connection | It does not stop the daemon (WM-INV-12). |

These methods throw `NOT_IMPLEMENTED` and never fall back to local SQLite:

- SAGE service: `unifiedSearchService`, `searchSageWithBreakdown`, `retrieveForAudience`, `findRelatedSage`, `graphFor`, `verify`, `verifyForPaths`, `hygiene`, `resolveCandidate`, `acceptCandidate`, `rejectCandidate`, `updateSage`, `deleteSage`, `recoverSage`, `backfillRecoverable`, `findMemoriesForFile`, `listSagePage`, `recordInjection`, `recordUse`, `getHqSyncVersion`, `listHqSync`, `applyHqSync`.
- Legacy `MemoryStore`: `readAll`, `read`, `remember`, `forget`, `consolidate`, `clear`, `list`, `search`, `findRelated`, `scoreRelevant`.

`withTraceId` returns the same port; trace correlation is not part of the wire protocol yet.

## Checks

```powershell
bun run typecheck          # from the repo root
bun run test:client        # contract parity script + bun test (integration uses target/debug/wrongmemoryd.exe)
```
