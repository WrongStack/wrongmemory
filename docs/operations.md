# Operations guide (Windows, per-user)

All commands accept `--data-root <dir>` (default `WRONGMEMORY_HOME`, else `%USERPROFILE%\.wrongmemory`) and `--json`. JSON goes to stdout and diagnostics to stderr.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Failed (e.g. disk full, corrupt store) |
| 2 | Invalid input (bad argument, output exists, unknown project or id) |
| 3 | Service unavailable (daemon not running or not reachable) |
| 4 | Unsupported (profile, method, or platform) |
| 5 | Denied (credential, grant profile, or read-only store) |
| 6 | Conflict (revision, idempotency key, owner, store identity, or running daemon) |
| 7 | Timeout |
| 70 | Internal error |

## Install

1. Unzip `wrongmemory-<version>-windows-x64.zip` and verify it first: `Get-FileHash <zip>` must match `SHA256SUMS`. The binaries are **unsigned**.
2. Run `bin\wrongmemory.exe install`. This copies `wrongmemory.exe` and `wrongmemoryd.exe` to `%LOCALAPPDATA%\WrongMemory\bin` (override with `--prefix`) and writes `install.json`. Nothing else on the system is changed.
3. Optional: `wrongmemory autostart enable` starts the daemon at sign-in. It adds a per-user `Run` value named `WrongMemory`; `autostart status` shows it, `autostart disable` removes it. Enabling autostart never opens a GUI and never enables memory capture.
4. Run `wrongmemory daemon start`, then `wrongmemory doctor`.

The installed runtime needs no Bun, Node, or source checkout.

### From WrongStack

WrongStack can install WrongMemory and switch a profile to it:

```text
wstack sage service install [--release <v>] [--from <dir>] [--stop-daemon]
wstack sage service use wrongmemory [--yes]    # back up, hand the store off, set the mode
wstack sage service use local [--yes]          # release the store again
wstack sage service status
```

- `install` downloads `wrongmemory-windows-x64.exe` and `wrongmemoryd-windows-x64.exe` from the release. It checks both against `SHA256SUMS` and against the digest GitHub recorded for the asset. It then runs this `install` command, so the binaries end up in the same directory. `--from <dir>` installs from a local build instead.
- WrongStack finds the binaries in this order: `Sage.service.daemonPath`, then `WRONGMEMORYD_PATH`, then `%LOCALAPPDATA%\WrongMemory\bin`, then next to the WrongStack executable, then `PATH`.
- `Sage.service.mode` is a profile setting, so it applies to every project of that profile.
  - Run `use wrongmemory` in a project to back up and hand over its existing store.
  - Other projects are registered the first time they are used.

## Update and rollback

- `wrongmemory install --stop-daemon` (run from the new version's `bin`) drains and stops the running daemon, then replaces both binaries atomically. The previous binaries are kept as `*.exe.old`. Without `--stop-daemon`, install refuses with exit 6 while a daemon is running.
- Binary rollback: stop the daemon, rename `wrongmemory*.exe.old` back over `wrongmemory*.exe`, then start the daemon.
  - Data and binary rollback are separate. An older binary refuses a newer registry (`registry schema … is newer`) instead of downgrading it.
  - Never restore an old memory backup just to roll back binaries; that discards newer memories.

## Uninstall

- `wrongmemory uninstall` stops the daemon, which releases store ownership. It removes the `Run` value if it points into the prefix, then removes the installed binaries, the `.old` copies, and `install.json`.
- Memories, grants, logs, and configuration are kept.
- `wrongmemory uninstall --remove-data --yes` also deletes the data root: registry, grants, logs, and **service-managed stores** under `projects\`. It refuses a directory that does not look like a WrongMemory data root.
- Adopted project stores (`<project>\.wrongstack\memories\sage.db`) are never deleted.
- Client configuration entries are removed separately with `wrongmemory disconnect <client> --project <id> [--revoke]`.

## Projects

| Command | Effect |
|---|---|
| `wrongmemory project add <root> --managed` | Creates a service-managed store for a project with no SAGE store |
| `wrongmemory project add <root>` | Adopts the existing SAGE store **read-only**; no bytes change |
| `wrongmemory project add <root> --write` | Requests write ownership. It stays read-only while `server.json` shows a possibly running WrongStack SAGE daemon; stop that daemon and add `--force-owner` |
| `wrongmemory project list` / `project show <id>` | Shows the store path, kind, admission, and owner epoch |
| `wrongmemory project detach <id>` | Releases ownership and disables the registration. The store file is kept |
| `wrongmemory project export <id>` | Managed stores only: copies the store to `<root>\.wrongstack\memories\sage.db` and detaches the project (rollback to in-process SAGE). Refuses an occupied location |

Run `wrongmemory migrate inspect <store>` before adopting. It is read-only and prints what admission would do.

### Semantic recall (optional)

| Command | Effect |
|---|---|
| `wrongmemory project semantic <id>` | Shows the provider (`none` by default) and how many memories are indexed |
| `wrongmemory project semantic <id> --provider ngram-hash` | Builds the vector cache and enables fused search for MCP clients |
| `wrongmemory project semantic <id> --provider none` | Disables it and deletes the cache |

- `ngram-hash` is the only built-in provider. It compares hashed character n-grams (sub-word similarity). It is offline and needs no model download, but it is not a neural embedding model. It mainly finds matches the lexical index cannot: substrings of unspaced Chinese/Japanese text, inflected and compound words.
- The cache lives in `<data root>\cache\semantic\<projectId>.sqlite`, never inside `sage.db`. It is derived data, rebuilt on demand, and deleted on disable, detach and export.
- The MCP `memory_search` tool fuses when a provider is set and stays lexical otherwise. IPC callers choose per request with `semantic`: `lexical` (default), `auto`, or `required` (fails with `SEMANTIC_UNAVAILABLE` when no provider is set). The WrongStack adapter keeps its own vector fusion and asks for `lexical`, so results are never fused twice.
- The vector channel never widens visibility: a vector hit is returned only if the caller could see the record through ordinary search (session, status, `contextPolicy`), and only at the revision the vector was built from.

## Dashboard

`wrongmemory ui` opens a read-only dashboard in the browser. It shows the daemon status, the projects with memory counts, each project's memories (search, status and kind filters, full record), candidates, live activity, audit trail, health check, maintenance schedule and semantic provider, and the client grants.

- It only reads. The page can call read methods only; the health check never applies repairs.
- It is local to this user: it listens on `127.0.0.1` only and opens with a fresh one-time link printed on start. The first visit trades it for a session cookie (`HttpOnly`, `SameSite=Strict`) and the link stops working; to open the dashboard in another browser, restart `wrongmemory ui`. Requests naming another host are refused, and grant secrets and the host token never reach the page.
- `--port <n>` picks the port (default: a free one); `--no-open` prints the link without opening a browser. Ctrl+C stops it; the daemon keeps running.
- Session-private memories are not listed: the dashboard has no session.

## Backup and restore

- `wrongmemory backup <project-id> --output <file>` takes a consistent online snapshot through the SQLite backup API from a read-only connection. It includes committed WAL content, runs `integrity_check`, and reports a SHA-256. It works while the daemon is running and never overwrites `<file>`.
- `wrongmemory backup --store <sage.db> --output <file>` does the same offline, without the daemon.
- Backups are plain SQLite and **not encrypted**. Store them like the source.
- Restoring is a deliberate manual step:
  1. `wrongmemory daemon stop`.
  2. Rename the current `sage.db`, `sage.db-wal` and `sage.db-shm` aside. Keep them.
  3. Copy the backup to `sage.db`.
  4. Check it with `wrongmemory migrate inspect <sage.db>`.
  5. `wrongmemory daemon start`.

## Connecting coding tools

```text
wrongmemory connect <claude-code|codex|cursor|antigravity> --project <id> --create-grant [--scope project|user] [--dry-run] [--with-guidance]
wrongmemory connect print --project <id> --grant <grant-id> --format json|toml
wrongmemory client doctor <client> --project <id>
wrongmemory disconnect <client> --project <id> [--revoke]
wrongmemory grants list | grants revoke <grant-id>
```

- Client configuration contains only the executable path, the project id, and the grant id.
- The grant secret stays in `<data-root>\grants\` and never leaves it.
- The default grant profile is `read-propose`.

### Optional loopback HTTP (WM-308)

Stdio (`wrongmemory mcp`) is the default and recommended transport. For a tool that only speaks MCP over HTTP:

```text
wrongmemory http serve [--port <port>]        # stays in the foreground; Ctrl+C stops it
wrongmemory http header --grant <grant-id>    # prints the Authorization header for that grant
```

- The endpoint is `http://127.0.0.1:<port>/mcp/<project-id>`. It listens on loopback only and is off unless you run it.
- Every request needs `Authorization: Bearer <grant-id>.<token>`. The token is derived from the grant secret (HMAC). The secret itself is never sent over HTTP. Revoking the grant, or re-issuing its secret, rejects the token on the next request.
- The token is a credential. Do not put it in a client config file. Use the client's header-helper mechanism to run `wrongmemory --json http header --grant <id>` when it connects, for example Claude Code's `headersHelper`.
- The grant decides which projects can be reached. Changing the project id in the URL does not widen it, and an unknown project gets the same refusal as a forbidden one.
- Requests with a foreign `Host` or with any browser `Origin` are refused.
- Bodies over 1 MiB are refused. At most 64 requests are served at once. Each grant gets 600 requests per minute.
- Both MCP 2026-07-28 and older revisions are served without transport sessions; no `Mcp-Session-Id` is issued.
- The listener is a CLI process in front of the daemon, not part of the daemon.

## Diagnostics

- `wrongmemory doctor` is a read-only health and store inspection.
- `wrongmemory diagnostics export --output <dir>` writes a bounded bundle: versions, daemon health, the project and admission list, store inspection counts, a grant summary without secrets, and the last 500 daemon log lines with secret-like runs redacted.
- The bundle contains **no memory text and no credentials**. Review it before sharing.

## Daemon

- `wrongmemory daemon start|status|stop`.
- The daemon is a per-user singleton on a private named pipe. Closing a client or MCP bridge never stops it.
- `stop` drains in-flight mutations and releases store ownership.
- Logs are in `<data-root>\logs\wrongmemoryd.log`, rotated at 5 MiB.
