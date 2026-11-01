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

## Diagnostics

- `wrongmemory doctor` is a read-only health and store inspection.
- `wrongmemory diagnostics export --output <dir>` writes a bounded bundle: versions, daemon health, the project and admission list, store inspection counts, a grant summary without secrets, and the last 500 daemon log lines with secret-like runs redacted.
- The bundle contains **no memory text and no credentials**. Review it before sharing.

## Daemon

- `wrongmemory daemon start|status|stop`.
- The daemon is a per-user singleton on a private named pipe. Closing a client or MCP bridge never stops it.
- `stop` drains in-flight mutations and releases store ownership.
- Logs are in `<data-root>\logs\wrongmemoryd.log`, rotated at 5 MiB.
