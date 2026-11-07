# WrongMemory

A per-user local memory service that keeps WrongStack's SAGE knowledge (structured memories, anchors, review queue, provenance) usable outside WrongStack. One Rust daemon serves many projects. WrongStack connects through a native adapter; other coding tools connect through MCP.

> **Status: preview, not certified.** The daemon, policy, native SAGE-profile storage, MCP bridge and WrongStack adapter are implemented and tested against an oracle built from the real SAGE code. They have run in real environments: WrongStack on a live store, Claude Code, Codex, Antigravity and MCP Inspector. Releases are unsigned; every asset has a SHA-256 checksum and a SLSA provenance attestation. The [1.0 evidence matrix](docs/evidence/wm-706-evidence-matrix.md) lists what passed and what is still open, and [docs/status.md](docs/status.md) lists every task with its evidence and what is still NOT RUN. The full program is in [docs/plan/](docs/plan/README.md).

## What works

- **Daemon.** `wrongmemoryd` runs one daemon per user and data root.
  - It listens on a named pipe (Windows) or a Unix socket (Linux, macOS) that only the owner can open.
  - Before a client sends any credential, the daemon must prove it holds the host token (an HMAC challenge), which defeats pipe squatting.
  - Clients authenticate with frame limits enforced, and there is one serialized writer lane per project.
  - Durable maintenance jobs and an event feed are filtered by visibility.
- **SAGE v5 `sage.db` compatibility.**
  - A read-only inspector admits or refuses a store.
  - Managed stores get DDL identical to SAGE's.
  - Existing WrongStack stores can be adopted. A linked Git worktree resolves to its main checkout's store.
- **Retrieval parity.** These match the SAGE oracle in order, not just in result set:
  - lexical search, including AND-then-OR matching, the NFKC `LIKE` fallback and the hybrid rerank;
  - path retrieval with graph expansion;
  - exact-ID reads;
  - list cursors;
  - related and graph traversal with hub damping.

  Session visibility is applied before `LIMIT`, counts and graph expansion.
- **Lifecycle parity.**
  - `remember` with exact and near-duplicate merge, supersedes and contradicts.
  - `update` patches the raw JSON with a revision guard.
  - Feedback and usage counters.
  - Tombstone delete with recovery and backfill.
  - Hygiene with protection for critical memories.
  - Transactional candidate accept, reject and resolve.
  - Every mutation carries a durable idempotency receipt, so a retry after a crash returns the original result.
- **Policy.**
  - Grants use one of the profiles `read-only`, `read-propose`, `writer` or `curator`, with a project allowlist.
  - Revocation is checked on every request.
  - Grant clients always get a daemon-issued session.
  - Client-supplied project, session or source fields never grant authority.
- **MCP.** `wrongmemory mcp` is a stdio bridge built on the official `rmcp` SDK.
  - Tools are listed according to the grant's profile.
  - The bridge starts the daemon by itself, so WrongStack does not need to run.
- **Client setup.**
  - `wrongmemory connect` plans owned entries for Claude Code, Codex, Cursor and Antigravity, or prints one.
  - `disconnect` removes those entries and revokes the grant.
  - `client doctor` checks the setup.
- **Operations.** `backup` uses the SQLite online backup API. Also: `migrate inspect`, `diagnostics export`, per-user `install`/`uninstall` and `autostart`. See [docs/operations.md](docs/operations.md) and [docs/runbooks.md](docs/runbooks.md).
- **TypeScript client.** `packages/client-ts` (`@wrongmemory/client`) mirrors the Rust contracts; a parity check runs in CI.

## Install

Releases ship Windows x64, Linux x64 and macOS arm64 builds: https://github.com/WrongStack/wrongmemory/releases/latest. The binaries are unsigned; each has a SHA-256 checksum and a GitHub build provenance attestation.

```powershell
# Windows (PowerShell 5.1 or 7)
irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.ps1 | iex
```

```sh
# Linux x64, macOS arm64
curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.sh | sh
```

The installers download `wrongmemory` and `wrongmemoryd` from the latest release, check both against `SHA256SUMS`, and run `wrongmemory install`. They do not start the daemon. The install directory goes on your `PATH`, so a new terminal finds `wrongmemory`:

- Windows: appended to the user `Path` (existing `%VAR%` entries and the value type are kept).
- Linux, macOS: a marked block in `~/.profile` and in your shell's startup file (`~/.bashrc`, `~/.zshrc`, or `~/.config/fish/conf.d/wrongmemory.fish`). The script prints the `export` line for the terminal you ran it in.

Run the same command again to update; nothing is added twice. A running daemon is left alone unless `WRONGMEMORY_STOP_DAEMON=1` is set. Other settings: `WRONGMEMORY_VERSION`, `WRONGMEMORY_INSTALL_DIR`, `WRONGMEMORY_AUTOSTART=1`, `WRONGMEMORY_VERIFY_ATTESTATION=1` (needs a signed-in `gh`), `WRONGMEMORY_NO_MODIFY_PATH=1` (leave `PATH` and startup files alone). `wrongmemory uninstall` removes the `PATH` entry again. Read [install.ps1](install.ps1) or [install.sh](install.sh) first if you prefer.

Or by hand, after checking the archive against `SHA256SUMS` (or with `gh attestation verify <file> --repo WrongStack/wrongmemory`):

```text
# Windows: unzip wrongmemory-<version>-windows-x64.zip
bin\wrongmemory.exe install        # copies both binaries to %LOCALAPPDATA%\WrongMemory\bin

# Linux, macOS: unpack wrongmemory-<version>-<platform>.tar.gz
bin/wrongmemory install            # copies both binaries to ~/.local/share/wrongmemory/bin

# Builds after 0.1.9 also put that directory on PATH (--no-modify-path to skip).

wrongmemory autostart enable       # optional: start the daemon at sign-in
wrongmemory doctor
```

`wrongmemory install` changes nothing else on the system and does not edit `PATH`: add the directory to `PATH` or call the binaries by full path.

### Use it instead of SAGE in WrongStack

```text
wstack sage service install                 # download, verify and install the latest release
wstack sage service use wrongmemory --yes   # switch the profile; back up and hand over this project's store
wstack sage service status
wstack sage service use local --yes         # hand the store back to WrongStack's own SAGE
```

WrongStack 1.4.0 installs the Windows build. Installing on Linux and macOS from inside WrongStack needs a WrongStack release with WrongStack/WrongStack#434. Until then, install from the archive as above; WrongStack finds the binaries there once that PR is in, and through `Sage.service.daemonPath`, `WRONGMEMORYD_PATH` or `PATH` before. Details, the lookup order and worktree behavior: [docs/operations.md](docs/operations.md#from-wrongstack).

## Quick start

```powershell
$wm = "$env:LOCALAPPDATA\WrongMemory\bin\wrongmemory.exe"   # or .\target\release\wrongmemory.exe after `cargo build --release`

& $wm project add C:\src\my-project --managed      # starts the daemon on demand
& $wm project list
& $wm remember <project-id> "Use bun run for scripts" --kind convention --file package.json
& $wm search <project-id> "bun scripts"

# External coding tool: create a read/propose grant and print its MCP entry
& $wm grants create --label codex --project <project-id>
& $wm connect print --project <project-id> --grant <grant-id> --format toml   # or json
```

- **Adopting a store.** `project add <root>` adopts an existing WrongStack store, read-only by default. Write ownership needs `--write`. The store stays read-only while a legacy SAGE daemon may be running. Back up a live store before adopting it for writes.
- **Client config.** `connect print` only prints an entry. `connect <client>` edits the client's config only when you ask it to, and only its own marked entry. The grant secret stays under `~/.wrongmemory/grants/` and never appears in any config.

## Benchmark

```powershell
cargo build --release -p wrongmemoryd -p wrongmemory-bench
target\release\wrongmemory-bench --corpus s   # or m (25,000 memories)
```

The benchmark starts a real daemon on a temporary data root and loads a deterministic synthetic corpus. It reports:

- p50/p95/p99 latency, measured through IPC, for search, path and propose;
- daemon RSS;
- synthetic recall@5;
- the session leak count, which must be 0.

Results are in `benchmarks/results/`.

## Layout

| Path | Contents |
|---|---|
| `crates/wrongmemory-contracts` | Wire envelopes, typed errors, policy profiles, SAGE record DTOs |
| `crates/wrongmemory-storage` | Inspector, SAGE DDL, retrieval, mutations, candidates, recovery, hygiene, backup, owner guard |
| `crates/wrongmemory-registry` | `registry.db`: projects, grants, owner epochs, jobs and schedules |
| `crates/wrongmemory-ipc` | NDJSON framing, private named pipe (Windows) or Unix socket, discovery, server proof, client |
| `crates/wrongmemoryd` | Daemon: auth, authorization, dispatch, scheduler, events, fault hooks (debug builds only) |
| `crates/wrongmemory-cli` | `wrongmemory` CLI, MCP stdio bridge, client connect planner, install/ops |
| `crates/wrongmemory-bench` | Latency, resource and synthetic recall benchmark |
| `packages/client-ts` | TypeScript client used by the WrongStack adapter |
| `scripts/` | Bun scripts: WrongStack baseline, SAGE oracle fixtures, inventories, release packaging |
| `contracts/` | Generated storage profile, record shape and known-answer tests, inventories |
| `tests/fixtures/` | Read and mutation oracle stores built by the real SAGE code |

## Checks

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bun run test:client      # TS client tests + contract parity
```

## License

MIT
