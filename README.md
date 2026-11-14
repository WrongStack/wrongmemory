# WrongMemory

A per-user local memory service that keeps WrongStack's SAGE knowledge (structured memories, anchors, review queue, provenance) usable outside WrongStack. One Rust daemon serves many projects. WrongStack connects through a native adapter; other coding tools connect through MCP.

> **Status: preview, not certified.** The daemon, policy, native SAGE-profile storage, MCP bridge and WrongStack adapter are implemented and tested against an oracle built from the real SAGE code. They have run in real environments: WrongStack on a live store, Claude Code, Codex, Antigravity and MCP Inspector. Releases are unsigned; every asset has a SHA-256 checksum and a SLSA build provenance attestation. The [1.0 evidence matrix](docs/evidence/wm-706-evidence-matrix.md) lists what passed and what is still open. [docs/status.md](docs/status.md) lists every task with its evidence and what is still NOT RUN. The full program is in [docs/plan/](docs/plan/README.md).

## Platforms

| Platform | CLI + daemon + MCP bridge | Native desktop (preview) |
|---|---|---|
| Windows x64 | yes | yes (since 0.2.1) |
| Linux x64, glibc 2.34 or newer (no musl) | yes | source build; release bundle from the next release |
| macOS arm64 (Apple silicon) | yes | source build (CI only); release bundle from the next release |

Latest release: https://github.com/WrongStack/wrongmemory/releases/latest. Nothing else is needed at runtime: no Bun, Node or source checkout. On Windows on ARM the installer uses the x64 build under emulation, which has not been tested. The macOS build has run on hosted CI runners only; Linux also in WSL Ubuntu. [docs/status.md](docs/status.md) lists what is NOT RUN per platform.

## Install

```powershell
# Windows: native desktop + CLI + daemon (PowerShell 5.1 or 7)
irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install-desktop.ps1 | iex

# Windows: CLI + daemon only
irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.ps1 | iex
```

```sh
# Linux x64, macOS arm64
curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.sh | sh

# Linux x64, macOS arm64: native desktop + CLI + daemon (needs a release that ships the bundle)
curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.sh | WRONGMEMORY_DESKTOP=1 sh
```

What the installers do:

1. Download `wrongmemory` and `wrongmemoryd` (with the desktop: the desktop ZIP on Windows, the desktop tar.gz on Linux and macOS) from the release. Each file must match its single entry in `SHA256SUMS`, or nothing is installed. The desktop archive's extraction paths and manifest hashes are checked as well; links and paths outside the archive are refused.
2. Run `wrongmemory install`, which copies the binaries into a per-user directory and writes `install.json`. No administrator rights are needed.
   - Windows: `%LOCALAPPDATA%\WrongMemory\bin`.
   - Linux, macOS: `$XDG_DATA_HOME/wrongmemory/bin`, else `~/.local/share/wrongmemory/bin`.
3. Put that directory on your `PATH`, so a new terminal finds `wrongmemory`.
   - Windows: appended to the user `Path`. Existing `%VAR%` entries and the value type are kept.
   - Linux, macOS: a marked block in `~/.profile` and in your shell's startup file (`~/.bashrc`, `~/.zshrc`, or `~/.config/fish/conf.d/wrongmemory.fish`). The script prints the `export` line for the terminal you ran it in.
4. A daemon that is running is drained, stopped and started again from the new binaries. One that is not running is not started. No desktop window opens and no store is adopted.

Read [install.ps1](install.ps1), [install-desktop.ps1](install-desktop.ps1) or [install.sh](install.sh) first if you prefer.

### Options

Set these in the environment before running an installer:

| Variable | Effect |
|---|---|
| `WRONGMEMORY_VERSION=0.2.1` | Install that release instead of the latest. The desktop bundle exists from 0.2.1. |
| `WRONGMEMORY_INSTALL_DIR=<dir>` | Install somewhere else. |
| `WRONGMEMORY_AUTOSTART=1` | Start the daemon at sign-in (Windows `Run` value, systemd user unit, or macOS LaunchAgent). |
| `WRONGMEMORY_VERIFY_ATTESTATION=1` | Also verify the GitHub build provenance of each download. Needs a signed-in `gh`. |
| `WRONGMEMORY_NO_MODIFY_PATH=1` | Leave `PATH` and the shell startup files alone. |
| `WRONGMEMORY_DOWNLOAD_BASE=<url>` | Download from a mirror serving the release assets. |

```powershell
$env:WRONGMEMORY_VERSION = '0.2.1'; irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.ps1 | iex
```

```sh
curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.sh | WRONGMEMORY_VERSION=0.2.1 sh
```

### Manual or offline install

Download the archive for your platform from the release. Check it against `SHA256SUMS` and its provenance attestation:

```sh
gh attestation verify <file> --repo WrongStack/wrongmemory \
  --signer-workflow WrongStack/wrongmemory/.github/workflows/release.yml \
  --source-ref refs/tags/v0.2.1
```

Then unpack it and install from its `bin` directory:

```text
# Windows: wrongmemory-<version>-windows-x64.zip
bin\wrongmemory.exe install

# Windows desktop: wrongmemory-desktop-<version>-windows-x64.zip
bin\wrongmemory.exe install --desktop

# Linux, macOS: wrongmemory-<version>-<platform>.tar.gz
bin/wrongmemory install

# Linux, macOS desktop: wrongmemory-desktop-<version>-<linux-x64|macos-arm64>.tar.gz
bin/wrongmemory install --desktop
```

`install` takes `--prefix <dir>` and `--no-modify-path`. With `--desktop` it checks the bundle's version and every manifest hash before it stops a daemon or replaces a file.

### Update

Run the same installer command again; nothing is added to `PATH` twice. The previous binaries are kept as `*.old` (`*.exe.old` on Windows). An open desktop window keeps running its old code until you close and reopen it. MCP bridges reconnect to the new daemon by themselves. Rollback and data handling: [docs/operations.md](docs/operations.md#update-and-rollback).

### Uninstall

```text
wrongmemory uninstall
```

This stops the daemon and removes the binaries, the desktop and its license material, the Linux application-menu entry or macOS `WrongMemory.app`, the autostart entry and the `PATH` entries. Memory stores, grants and logs under `~/.wrongmemory` are kept. `wrongmemory uninstall --remove-data --yes` also deletes that data root, including the stores WrongMemory manages; adopted WrongStack stores are never touched.

## Quick start

```sh
wrongmemory doctor                                  # check the install and the daemon
wrongmemory project add ~/src/my-project --managed  # starts the daemon on demand
wrongmemory project list
wrongmemory remember <project-id> "Use bun run for scripts" --kind convention --file package.json
wrongmemory search <project-id> "bun scripts"
wrongmemory ui                                      # read-only dashboard on 127.0.0.1
```

- **Adopting a store.** `project add <root>` without `--managed` adopts an existing WrongStack store, read-only by default. Write ownership needs `--write`. The store stays read-only while a legacy SAGE daemon may be running. From 0.1.11 the daemon backs the store up to `~/.wrongmemory/backups/` before it takes write ownership.
- **Data root.** `~/.wrongmemory` by default; `WRONGMEMORY_HOME` or `--data-root` changes it.

### Connect a coding tool

```sh
# Plan the entry first, then write it. Claude Code, Codex, Cursor and Antigravity are supported.
wrongmemory connect claude-code --project <project-id> --create-grant --dry-run
wrongmemory connect claude-code --project <project-id> --create-grant

# Any other MCP client: create a grant and print an entry to paste
wrongmemory grants create --label my-tool --project <project-id>
wrongmemory connect print --project <project-id> --grant <grant-id> --format toml   # or json

wrongmemory client doctor                           # check the setup
wrongmemory disconnect claude-code --project <project-id>
```

- `connect <client>` edits the client's configuration only when you ask it to, and only its own marked entry. `disconnect` removes that entry and revokes the grant.
- The grant secret stays under `~/.wrongmemory/grants/`. Client configurations carry only the project ID and grant ID.
- New grants use the `read-propose` profile: the tool can read and propose candidates, and you review them. Certification is per client and version; see [docs/status.md](docs/status.md).

### Use it instead of SAGE in WrongStack

```text
wstack sage service install                 # download, verify and install the latest release
wstack sage service use wrongmemory --yes   # switch the profile; back up and hand over this project's store
wstack sage service status
wstack sage service use local --yes         # hand the store back to WrongStack's own SAGE
```

WrongStack 1.4.0 installs the Windows build only. WrongStack 1.5.6 includes #434's Linux and macOS installer and integration changes (verified 2026-11-11). Lookup order and worktree behavior: [docs/operations.md](docs/operations.md#from-wrongstack). Current certification gaps, and the difference between the native integration and external MCP, are in the [readiness review](docs/readiness-review-2026-11-11.md).

## Desktop (preview: Windows, Linux, macOS)

`wrongmemory-desktop` is an optional native GPUI workbench on the same authenticated service. Every machine runs one per-user daemon; the desktop, WrongStack and MCP clients are all clients of it. The desktop owns no database, and closing it leaves the daemon and other clients running. It has a paged library, a full-text evidence inspector, revision-checked edits, review decisions, a relations view, client grants, health/jobs and a command palette (Ctrl+K, Cmd+K on macOS). Installation never opens it. `install --desktop` adds an application-menu entry with an icon on Linux and `~/Applications/WrongMemory.app` on macOS; on Windows start `wrongmemory-desktop` yourself (no Start Menu shortcut yet).

One code base builds for all three platforms. Windows is published since 0.2.1. Linux has been built, tested against real daemons, packaged, installed and launched under WSLg (X11); macOS builds and tests run in hosted CI only. Release bundles for Linux x64 and macOS arm64 ship from the next release. Accessibility, IME, high-DPI, GPU fallback, sleep/resume, native Wayland rendering, macOS rendering and sustained frame performance are NOT RUN. Details: [apps/desktop/README.md](apps/desktop/README.md).

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
- **Client setup.** `connect`, `disconnect` and `client doctor` (see [Connect a coding tool](#connect-a-coding-tool)).
- **Operations.** `backup` uses the SQLite online backup API. Also: `migrate inspect`, `diagnostics export`, per-user `install`/`uninstall`, `autostart` and the read-only `ui` dashboard. See [docs/operations.md](docs/operations.md) and [docs/runbooks.md](docs/runbooks.md).
- **Desktop.** The cross-platform preview above.
- **TypeScript client.** `packages/client-ts` (`@wrongmemory/client`) mirrors the Rust contracts; a parity check runs in CI.

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
| `packages/dashboard` | Read-only browser dashboard served by `wrongmemory ui` |
| `apps/desktop` | Optional native GPUI desktop: separate Rust 1.99.1 workspace and lockfile (the core stays on Rust 1.93.0) |
| `scripts/` | Bun scripts: WrongStack baseline, SAGE oracle fixtures, inventories, release and desktop packaging |
| `contracts/` | Generated storage profile, record shape and known-answer tests, inventories |
| `tests/fixtures/` | Read and mutation oracle stores built by the real SAGE code |
| `install.ps1`, `install-desktop.ps1`, `install.sh` | Release installers, tested against the published release in CI |

## Development

```powershell
cargo build --release                 # target\release\wrongmemory(.exe) and wrongmemoryd(.exe)
bun run desktop:run                   # build and run the desktop from source
bun run desktop:package               # desktop bundle for this platform (ZIP on Windows, tar.gz on Linux/macOS)
```

Checks:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bun run test:client          # TS client tests + contract parity
bun run typecheck
bun run desktop:check        # desktop fmt, Clippy and tests (Rust 1.99.1)
bun run desktop:package:test
```

Contributor rules, including the store-safety rules, are in [AGENTS.md](AGENTS.md).

## License

MIT
