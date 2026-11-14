# WM-ADR-014: Independent optional native desktop workspace

Date: 2026-11-11. Status: accepted for the Windows desktop preview.

The authorized native GUI uses `apps/desktop`, an independent Cargo workspace with its own lockfile, exact dependency pins and Rust 1.99.1 toolchain. The core workspace, contracts and IPC remain on Rust 1.93.0. No storage ownership or protocol invariant changes.

## Evidence and decision

The exact GPUI Kit 0.9.1 dependency family selects gpui-pre / gpui-pre-platform 0.3.12. Rust 1.93 resolves it but fails to compile upstream calls to `std::hint::cold_path` (E0658). The isolated Rust 1.99.1 preflight passes. A newer toolchain for an optional presentation client does not justify changing the independently supported core toolchain.

The desktop depends on the existing contracts and IPC crates by path, never storage or SQLite. A bounded service thread carries typed commands. The existing CLI performs explicit daemon activation; desktop code must not duplicate detached daemon startup or stop the service on exit. Unknown mutation outcomes are displayed and not automatically retried with new intent keys.

## Required delivery boundaries

- Desktop CI runs its own format, all-target Clippy, unit and real-daemon tests, five consecutive daemon integration runs and executable build on Windows. Core CI remains required.
- Release automation packages the Windows desktop separately with matching core CLI/daemon versions, hashes, source identity, dependency inventory and available license texts. The publish job depends on that build; no new publication permission is granted to build jobs.
- The browser and headless distributions remain independent. Linux/macOS native support, signing, accessibility and performance are separate acceptance gates.
- Versions are aligned by the package script; no silent desktop/core mismatch. Future framework updates must update exact pins and the lockfile and repeat the desktop gate.

This records the workspace strategy and delivered source; it does not certify native 1.0, an installed update path, or external MCP clients. See [native product program](native-desktop-gpui.md) and [current status](../status.md).

## Addendum 2026-11-14: Linux and macOS targets

Status: accepted. The same `apps/desktop` workspace, toolchain and exact GPUI Kit 0.9.1 pins now target Linux x64 and macOS arm64 as well as Windows x64. No storage, contract or IPC change; the desktop remains a client of the one per-user daemon that WrongStack and MCP clients use.

- CI adds a `desktop-unix` job (ubuntu-latest, macos-latest): Clippy, tests, five consecutive real-daemon runs, package regression tests and the executable build. Format runs once on Windows.
- Release automation adds a `desktop-unix` job (ubuntu-22.04 for the glibc floor, macos-latest) that packages, verifies and re-runs the daemon suite against the extracted CLI; publish depends on it. Linux/macOS bundles are tar.gz to keep executable bits.
- `wrongmemory install --desktop` admits the executables of the platform it runs on (`bin/<name>` plus the platform suffix) and refuses bundles for another platform. `install.sh` gains `WRONGMEMORY_DESKTOP=1`.
- Supplementary license material covers the union of the Windows, Linux and macOS dependency graphs.
- Windows binaries link with an 8 MiB main-thread stack, matching Linux/macOS, because unoptimized GPUI element trees overflowed the default 1 MiB.

- Linux `install --desktop` writes an owned Desktop Entry and icon below `$XDG_DATA_HOME`; the window `app_id` / X11 `WM_CLASS` is `com.wrongstack.WrongMemory` to match `StartupWMClass`. CI requires `desktop-file-validate` to accept the generated entry. A macOS `.app` bundle and a Windows Start Menu shortcut are not implemented.

Linux desktop launch has been observed under WSLg/X11 only; native Wayland rendering and macOS rendering are NOT RUN until observed. Hosted CI results belong in `docs/status.md`.
