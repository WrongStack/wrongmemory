# WM-403: memory injection in a released WrongStack host, 2026-11-04

WM-403 asks for parity in what the model actually receives: tool-result and turn renders, and the use credit recorded for injected memories. This run drove the same released WrongStack host sessions in `wrongmemory` and `local` mode and compared every request the model received, line by line. It found one rendering difference, the key order of a returned record. That is fixed in 0.1.9.

The environment is the one from the WM-404 run (`docs/evidence/wm-404-capture-host-run-2026-11-04.md`): disposable homes and data roots, released `wstack` 1.4.0 and a scripted OpenAI-compatible provider that logs every request. WrongMemory was the attested v0.1.8 draft package, and then the fix built from the working tree.

## Setup

| Part | Value |
|---|---|
| Profiles | `Sage.inject.turnContext: true` (opt-in turn injection), `toolResults: true` (default), capture off; one profile in `wrongmemory` mode, one in `local` mode |
| Projects | fresh git repositories with the same `src/config.ts` and `README.md`, one per mode |
| Session 1 | `remember` through `tool_use`: a `file_note` anchored to `src/config.ts`, importance 0.8 |
| Session 2 | `read src/config.ts`, with a prompt about the service port |
| Session 3 | `read README.md`, with a question about the service port |

## Results

| Check | Result |
|---|---|
| Requests the model received | 16 in each mode, in the same order |
| Turn injection (`sage.turn-memory`, sessions 2 and 3) | Identical: the same `[file_note][high]` line with the memory id, anchor and historical hint |
| Tool-result evidence (memory companion, `unverified_anchor`) | Identical |
| Side requests (session summary with existing entries, evidence verification) | Identical |
| Use credit on the record after the three sessions | Identical: `injectionCount` 4, `lastAccessedAt` set, revision 1 |
| `remember` tool result, v0.1.8 | **Different**: the record's keys came in another order (`id revision scope kind status text …` against the baseline's `id revision text kind scope status …`), plus one line `wrongmemory: {"principalId":"host","client":"wrongstack"}` |
| `remember` tool result after the fix | Same order; only the `wrongmemory` line differs |

Apart from project paths, the full line diff of all requests after the fix shows only the `wrongmemory` line: 4 lines, where the result is echoed into later requests.

## The key order

WrongStack renders a returned record's scalar keys in their JSON order. The baseline builds a new record as `id, revision, text, kind, scope, status, tags, anchors, sources, …, importance, confidence, freshness, …, createdAt, updatedAt` (`sqlite-store-remember.ts`). WrongMemory serialized its typed record in its struct's field order, so records it created were stored and returned in another order.

The struct now declares its fields in the baseline's order. A new record's stored JSON then has the same key order as a record the baseline wrote. Keys added later by updates are appended, as before, because rewrites patch the stored JSON object. `new_records_keep_the_baseline_key_order` compares a new record's stored keys with a record from the oracle `basic` fixture. With the old field order, the test fails.

Records created before the fix keep their stored order: rewrites patch the stored object and never reorder it.

## Deliberate difference: the `wrongmemory` attribution

A record written through WrongMemory carries `wrongmemory: {principalId, client}`. It is the authenticated writer, taken from the connection and never from the client. A host that prints every key of a record shows it as one extra line. After `project detach`, the unmodified baseline read a store with such records and reported the same totals and doctor checks (WM-600).
