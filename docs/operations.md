# Operations guide (per-user: Windows, Linux, macOS)

All commands accept `--data-root <dir>` (default `WRONGMEMORY_HOME`, else `%USERPROFILE%\.wrongmemory` on Windows and `~/.wrongmemory` on Linux and macOS) and `--json`. JSON goes to stdout and diagnostics to stderr.

Windows is the primary platform: its packages, the live store handoff and the WrongStack integration were run there. Linux x64 and macOS arm64 run the same daemon, CLI and MCP bridge (WM-802); see [Linux and macOS](#linux-and-macos) for what differs and what has not been run.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Failed (e.g. disk full, corrupt store) |
| 2 | Invalid input (bad argument, output exists, unknown project or id) |
| 3 | Service unavailable (daemon not running or not reachable) |
| 4 | Unsupported (profile, method, or platform) |
| 5 | Denied (credential, grant profile, or read-only store). `wrongmemory mcp` exits 5 when its grant has no secret in the data root (revoked, or created under another data root) |
| 6 | Conflict (revision, idempotency key, owner, store identity, or running daemon) |
| 7 | Timeout |
| 70 | Internal error |

## Install

For the optional Windows native desktop, use `irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install-desktop.ps1 | iex`, or set `WRONGMEMORY_DESKTOP=1` before running `install.ps1`. The desktop bootstrap honors the same version/prefix/PATH/autostart/attestation options. It downloads the full Windows desktop ZIP, requires one matching SHA-256 entry, validates extraction paths and runs `install --desktop` from the extracted bundle. The CLI checks the bundle version and every manifest hash before stopping a daemon or replacing executables. Offline: `bin\wrongmemory.exe install --desktop` from the verified desktop ZIP.

The GUI installs beside the CLI/daemon; license and corresponding-source materials are kept under `<prefix>/desktop-resources`. No GUI launches during install and no live SAGE store is adopted. Run `wrongmemory-desktop` afterwards. Updating keeps previous executables as `.old`; an already-open GUI keeps its existing code and draft until you close/reopen it. A failed binary replacement restores previous executables where present. Binary rollback is separate from data rollback; do not restore a stale memory backup merely to downgrade an application.

`wrongmemory uninstall` removes the three executables, owned desktop resource files and its PATH/autostart entries. Unmanaged files under the resource directory are preserved. Close the desktop first: Windows can refuse removal of its running executable. Memory stores and settings remain unless `--remove-data --yes` is explicitly requested. Linux/macOS installers remain headless; native GUI support there is not certified.

The installers in the repository root do the work of steps 1 and 2 below for the latest release (or `WRONGMEMORY_VERSION`): `irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.ps1 | iex` on Windows, `curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.sh | sh` on Linux and macOS. They download the two bare executables, refuse any whose SHA-256 is not the single matching entry in `SHA256SUMS`, and run `wrongmemory install` with them. A daemon that is running is drained and stopped for the update and started again from the installed `wrongmemoryd` (`wrongmemory install` does this itself from 0.1.12, `--no-restart` leaves it stopped; for older releases the scripts pass `--stop-daemon` and run `daemon start` afterwards). Other WrongMemory processes (MCP bridges, `wrongmemory ui`) keep running the previous binary, which the install renames to `*.old` (or `*.old-<n>` when an earlier `*.old` still runs); they pick up the new version when their client starts them again, and the bridges reconnect to the new daemon by themselves. `WRONGMEMORY_AUTOSTART=1` runs step 3. `WRONGMEMORY_VERIFY_ATTESTATION=1` also runs `gh attestation verify` on each executable. The Windows installer appends the install directory to the user `PATH` (`REG_EXPAND_SZ`, other entries left unexpanded; `WRONGMEMORY_NO_MODIFY_PATH=1` skips it). The Unix installer adds a marked block to `~/.profile` and the shell's startup file (same opt-out); `wrongmemory uninstall` removes both.

1. Unzip `wrongmemory-<version>-windows-x64.zip` and verify it first: `Get-FileHash <zip>` must match `SHA256SUMS`. The binaries are **unsigned**.
2. Run `bin\wrongmemory.exe install`. This copies `wrongmemory.exe` and `wrongmemoryd.exe` to `%LOCALAPPDATA%\WrongMemory\bin` (override with `--prefix`) and writes `install.json`. From 0.1.10 it also updates the per-user PATH setup; `--no-modify-path` skips that step.
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

- `install` downloads `wrongmemory-<platform>` and `wrongmemoryd-<platform>` from the latest release, or the one named by `--release`. It checks both against `SHA256SUMS` and against the digest GitHub recorded for the asset, and marks only verified bytes executable. WrongStack 1.4.0 knows `windows-x64` only; WrongStack 1.5.6 includes #434 (merged 2026-11-05), which adds `linux-x64` and `darwin-arm64` (release/ancestry verified 2026-11-11). It then runs this `install` command, so the binaries end up in the same directory. `--from <dir>` installs from a local build instead.
- WrongStack finds the binaries in this order: `Sage.service.daemonPath`, then `WRONGMEMORYD_PATH`, then the install directory, then next to the WrongStack executable, then `PATH`. The install directory is `%LOCALAPPDATA%\WrongMemory\bin` on Windows. On Linux and macOS, with #434, it is `$XDG_DATA_HOME/wrongmemory/bin`, else `~/.local/share/wrongmemory/bin`; WrongStack 1.4.0 needs `daemonPath`, `WRONGMEMORYD_PATH` or `PATH` there.
- `Sage.service.mode` is a profile setting, so it applies to every project of that profile.
  - Run `use wrongmemory` in a project to back up and hand over its existing store.
  - Other projects are registered the first time they are used. From 0.1.11 the daemon copies an existing store to `<data root>/backups/<name>-before-write-<UTC time>.db` before it becomes its writer, whoever asked for write ownership (`writeBackup` in the `project.register` result; `project add --write` prints it). A failed copy refuses the registration.
  - A linked Git worktree uses its main checkout's project and store, as local SAGE does.
  - Memory is shared across worktrees; file evidence is not. A host session in a linked worktree binds its connection to that worktree (`project.bindCheckout`, from WrongMemory 0.1.4 and a WrongStack build that sends it). The daemon accepts only the project's own root or a worktree whose Git link checks out from both sides, and only from the host, never from a grant.
  - Once bound, absolute paths inside the worktree resolve to the shared project paths. `memory_verify` then checks the worktree's own files and labels each result `checkout: {kind: "linkedWorktree", root}`. Those results are reported, never written back: only the main checkout's verification (and scheduled hygiene) changes a memory's status or `lastVerifiedAt`. A file renamed on a branch therefore does not mark the shared memory stale.
  - External MCP clients always verify against the main checkout.
- `use local` detaches the project. A later `use wrongmemory` registers it again under a **new project id**: reconnect external clients (`wrongmemory connect … --create-grant`), since their entries carry the old id.

### Linux and macOS

1. Unpack `wrongmemory-<version>-linux-x64.tar.gz` or `wrongmemory-<version>-darwin-arm64.tar.gz` and verify it first: `sha256sum <archive>` (Linux) or `shasum -a 256 <archive>` (macOS) must match its line in `SHA256SUMS`. The binaries are **unsigned** and not notarized: on macOS, a file downloaded by a browser carries a quarantine flag, so Gatekeeper blocks it until you clear the flag (`xattr -d com.apple.quarantine <file>`) after verifying it. Files fetched with `curl` or `gh release download` carry no flag. The Linux binaries are built on Ubuntu 22.04 and need glibc 2.34 or newer (the newest symbol version they reference).
2. Run `bin/wrongmemory install`. This copies `wrongmemory` and `wrongmemoryd` to `$XDG_DATA_HOME/wrongmemory/bin`, else `~/.local/share/wrongmemory/bin` (override with `--prefix`), and writes `install.json`. Put that directory on `PATH` or call it by full path.
3. Optional: `wrongmemory autostart enable` starts the daemon at sign-in. Like the Windows `Run` value, it never starts the daemon now.
   - **Linux:** it writes the systemd user unit `~/.config/systemd/user/wrongmemory.service` (or under `$XDG_CONFIG_HOME`) and links it into `default.target.wants/`, which is what `systemctl --user enable` does. It works without a reachable user manager; when one is reachable, `systemctl --user daemon-reload` runs so `systemctl --user status wrongmemory` sees it. Exit status 3 (another daemon already serves the data root) counts as success.
   - **macOS:** it writes the LaunchAgent `~/Library/LaunchAgents/com.wrongstack.wrongmemory.plist` with `RunAtLoad`; launchd loads it at the next sign-in. `autostart disable` also unloads it from the running session.
4. Run `wrongmemory daemon start`, then `wrongmemory doctor`.

How the daemon differs on Unix:

- **Endpoint.** A Unix socket at `<data-root>/runtime/daemon.sock`, mode 0600, inside the owner-only (0700) `runtime/` directory. A `runtime/` directory owned by another account is refused. Socket paths are limited to 107 bytes on Linux and 103 on macOS; a data root too deep for that is refused with a hint to use a shorter one.
- **One daemon per data root.** An exclusive `flock` on `runtime/daemon.lock` elects it. The kernel drops the lock when the daemon exits, so a socket file left by a crash never blocks a restart.
- **Who may connect.** The daemon drops any peer whose uid is not its own, and the Rust client refuses a socket served by another uid before sending a credential. The TypeScript client (used by WrongStack) proves the server through the same HMAC challenge as on Windows.
- **Signals.** SIGTERM and SIGINT drain like `daemon stop`. SIGHUP is ignored. An on-demand start runs the daemon in its own session (`setsid`), with stdio on `/dev/null` and no inherited descriptors.
- **Update.** The previous binaries are kept as `wrongmemory.old` and `wrongmemoryd.old`.

Installing from inside WrongStack on Linux and macOS needs WrongStack/WrongStack#434. Its installer installed the published v0.1.9 on Linux in a throwaway home directory (2026-11-05); a released WrongStack with it has not been run. Not run on Linux or macOS yet: real MCP clients, a soak, an install on macOS outside hosted CI, and an autostart through a real systemd or launchd session (the tests write the unit and agent to a temporary directory).

## Update and rollback

- `wrongmemory install --stop-daemon` (run from the new version's `bin`) drains and stops the running daemon, then replaces both binaries atomically. The previous binaries are kept as `*.exe.old` (`*.old` on Linux and macOS). Without `--stop-daemon`, install refuses with exit 6 while a daemon is running.
- Binary rollback: stop the daemon, rename `wrongmemory*.exe.old` back over `wrongmemory*.exe` (`wrongmemory*.old` over `wrongmemory*`), then start the daemon.
  - Data and binary rollback are separate. An older binary refuses a newer registry (`registry schema … is newer`) instead of downgrading it.
  - Never restore an old memory backup just to roll back binaries; that discards newer memories.

## Uninstall

- `wrongmemory uninstall` stops the daemon, which releases store ownership. It removes the `Run` value (Windows), systemd user unit (Linux) or LaunchAgent (macOS) if it starts a daemon from the prefix, then removes the installed binaries, the `.old` copies, and `install.json`.
- Memories, grants, logs, and configuration are kept.
- `wrongmemory uninstall --remove-data --yes` also deletes the data root: registry, grants, logs, and **service-managed stores** under `projects\`. It refuses a directory that does not look like a WrongMemory data root.
- Adopted project stores (`<project>\.wrongstack\memories\sage.db`) are never deleted.
- Client configuration entries are removed separately with `wrongmemory disconnect <client> --project <id> [--revoke]`.

## Projects

| Command | Effect |
|---|---|
| `wrongmemory project add <root> --managed` | Creates a service-managed store for a project with no SAGE store |
| `wrongmemory project add <root>` | Adopts the existing SAGE store **read-only**; no bytes change |
| `wrongmemory project add <root> --write` | Requests write ownership. It stays read-only while the SAGE project server named in `server.json` still runs; a `server.json` whose pid no longer runs does not block. Run the same command again once that server is gone (or add `--force-owner` after stopping it) to upgrade a registration held read-only for this reason |
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

`wrongmemory ui` opens a management dashboard in the browser:

- **Projects:** every registered project with memory and review counts. Remove one or several (for example all empty ones). Removing is `project.detach`: the store file stays where it is, an adopted store goes back to local SAGE, and a WrongStack host in `wrongmemory` mode registers the project again the next time it is used there. Removing a project that holds memories asks you to type `remove`.
- **Memories:** search and filter, then open a memory to read it with its kind, status, importance, anchors, tags, lineage (replaces, replaced by, contradicts), related memories, sources and feedback. Edit text, kind, status, importance, confidence, persistence and tags; an edit names the revision it was made from, so a concurrent change by an agent is refused (`REVISION_CONFLICT`) instead of overwritten. Delete (a recoverable tombstone, recorded in the audit trail; a permanent memory needs a second confirmation) and recover.
- **Relations:** a graph (React Flow) centred on a memory, a file or directory path, or a topic: memories, the files, directories and symbols they are anchored to, and the replaces / contradicts / same-topic links between them.
- **Review:** accept or reject proposals one by one or in batches.
- **Activity:** live changes and the stored audit trail.
- **Health & maintenance:** the doctor report with a Fix button for each repairable check, the maintenance schedule, recent jobs, and "Run maintenance now".
- **Client grants** and **Daemon** status (read only).

Access rules:

- Reads go over `GET /api`. Changes go over `POST /api`, only for the list above, and each must carry the run's CSRF token (handed only to a request with the session cookie) and come from the page's own origin. Grant creation and revocation, export, registration, semantic configuration and daemon shutdown are not reachable from the page.
- It is local to this user: it listens on `127.0.0.1` only and opens with a fresh one-time link printed on start. The first visit trades it for a session cookie (`HttpOnly`, `SameSite=Strict`) and the link stops working; to open the dashboard in another browser, restart `wrongmemory ui`. Requests naming another host are refused, and grant secrets and the host token never reach the page.
- A read-only project (for example one held by legacy SAGE metadata) shows no edit buttons.
- `--port <n>` picks the port (default: a free one); `--no-open` prints the link without opening a browser. Ctrl+C stops it; the daemon keeps running.
- Session-private memories are not listed: the dashboard has no session.
- The page is built from `packages/dashboard` (React, React Flow, Tailwind) with `bun run --cwd packages/dashboard build`, which writes `crates/wrongmemory-cli/src/ui_assets/`. The built files are committed, so building the Rust workspace needs no Bun; rebuild them after changing the page.

## Backup and restore

- `wrongmemory backup <project-id> --output <file>` takes a consistent online snapshot through the SQLite backup API from a read-only connection. It includes committed WAL content, runs `integrity_check`, and reports a SHA-256. It works while the daemon is running and never overwrites `<file>`.
- `wrongmemory backup --store <sage.db> --output <file>` does the same offline, without the daemon.
- Write adoption takes such a backup by itself (see above). Those copies are not pruned.
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
wrongmemory connect codex --project <id> --create-grant --approve-proposals
wrongmemory connect print --project <id> --grant <grant-id> --format json|toml
wrongmemory client doctor <client> --project <id>
wrongmemory disconnect <client> --project <id> [--revoke]
wrongmemory grants list | grants revoke <grant-id>
```

- Client configuration contains only the executable path, the project id, the grant id, and the data root when it is not the default (`--data-root` or `WRONGMEMORY_HOME`; the client starts the bridge from its own environment, which may not have that variable).
- The grant secret stays in `<data-root>\grants\` and never leaves it.
- The default grant profile is `read-propose`.
- `--with-guidance` adds a short recall/propose rule to `CLAUDE.md` or `AGENTS.md`. When a recalled warning or decision conflicts with the request, the rule asks the agent to do what the user asked, and to name the record and its stated risk in its reply. The MCP server instructions say the same. In the WM-309 evaluation (`docs/evidence/wm-309-agent-eval-2026-11-03.md`), Codex without guidance never called a memory tool (0 of 4 runs). With guidance, every Claude Code and Codex run:
  - recalled;
  - proposed revisions rather than duplicates;
  - with the current wording, told the user about the conflicting warning and its risk.
- Codex asks before every MCP tool that is not read-only, and `codex exec` refuses such a call instead of asking. Proposals therefore fail in non-interactive Codex runs unless approved. `--approve-proposals` (Codex only, read-propose grants only) writes `approval_mode = "approve"` for `memory_candidates` and `memory_feedback`. The daemon still limits the grant to listing, proposing and feedback.

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
- The daemon is a per-user singleton per data root, on a private named pipe (Windows) or a private Unix socket (Linux, macOS). Closing a client or MCP bridge never stops it.
- `stop` drains in-flight mutations and releases store ownership.
- Logs are in `<data-root>/logs/wrongmemoryd.log`, rotated at 5 MiB.
