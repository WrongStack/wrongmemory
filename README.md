# WrongMemory

A per-user local memory service that keeps WrongStack's SAGE knowledge (structured memories, anchors, review queue, provenance) usable outside WrongStack. One Rust daemon serves many projects. WrongStack will connect through a native adapter; other coding tools connect through MCP.

> **Status: early preview (M0 + M1/M2/M3 vertical slice).** This is not a full SAGE replacement yet. See [docs/status.md](docs/status.md) for what works, what is verified, and the known gaps. The full program is in [docs/plan/](docs/plan/README.md).

## What works today

- `wrongmemoryd`: a single daemon per user and data root. A named pipe restricted to the current user is the singleton guard. Clients authenticate, and the daemon keeps running when a client disconnects.
- SAGE v5 `sage.db` compatibility:
  - A read-only inspector admits or refuses stores.
  - Managed stores are created with DDL identical to SAGE's. The test checks the oracle DDL fingerprint.
  - Existing WrongStack stores can be adopted read-only, without changing their bytes.
- Retrieval:
  - Lexical search ported from `searchSage`: FTS5 with AND-then-OR matching, plus the NFKC `LIKE` fallback.
  - Path lookup over anchors.
  - Exact-ID reads.
  - Session visibility is applied before `LIMIT`.
- Writes:
  - `remember` follows SAGE's projections: canonical/Unicode text, anchor graph edges, the HQ sync projection, and audit.
  - `candidate.propose` adds a review proposal.
  - Both use durable idempotency receipts, so a retried mutation returns the original result.
- Policy:
  - Scoped grants with the profiles `read-only`, `read-propose`, `writer` and `curator`.
  - Each grant has a project allowlist, and revocation applies to clients that are already connected.
  - Grant clients always get a daemon-issued session.
- `wrongmemory mcp`: an MCP stdio bridge built on the official `rmcp` SDK. It can start the daemon by itself and works without WrongStack running.

## Quick start (Windows)

```powershell
cargo build --release
$wm = ".\target\release\wrongmemory.exe"

& $wm project add C:\src\my-project --managed      # starts the daemon on demand
& $wm project list
& $wm remember <project-id> "Use bun run for scripts" --kind convention --file package.json
& $wm search <project-id> "bun scripts"

# External coding tool: create a read/propose grant and print its MCP entry
& $wm grants create --label codex --project <project-id>
& $wm connect print --project <project-id> --grant <grant-id> --format toml   # or json
```

Adopt an existing WrongStack store with `project add <root>`. Adoption is read-only by default. Write ownership needs `--write`, and it stays read-only while a legacy SAGE daemon may be running.

`connect print` only prints a configuration entry; nothing edits your client configuration. The grant secret stays under `~/.wrongmemory/grants/` and never appears in the config.

## Layout

| Path | Contents |
|---|---|
| `crates/wrongmemory-contracts` | Wire envelopes, typed errors, policy profiles, SAGE record DTOs |
| `crates/wrongmemory-storage` | Inspector, SAGE DDL, store reads and writes, owner guard, JS-compatible text rules |
| `crates/wrongmemory-registry` | `registry.db`: projects, store descriptors, grants, owner epochs |
| `crates/wrongmemory-ipc` | Newline-delimited JSON framing, private named pipe, discovery, client |
| `crates/wrongmemoryd` | Daemon: authentication, authorization, dispatch, per-project writer lanes |
| `crates/wrongmemory-cli` | `wrongmemory` CLI and MCP stdio bridge |
| `scripts/` | Bun scripts that capture the WrongStack baseline and build the SAGE oracle fixtures |
| `contracts/` | Generated storage profile, record shape and known-answer tests, writer inventory |
| `tests/fixtures/oracle/` | Stores built by the real SAGE code, with recorded query results |

## Checks

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## License

MIT
