# WrongMemory Desktop preview

A native Rust / GPUI Kit knowledge workbench for the existing WrongMemory service. Windows x64 is the initial build target. This optional client does not own SQLite, start a daemon automatically, install itself, or stop the daemon when its window closes.

## Run

Unpack the complete desktop ZIP and run `bin/wrongmemory-desktop.exe`. Keep the adjacent `wrongmemory.exe` and `wrongmemoryd.exe`: the explicit **Start service** button delegates to the existing CLI. An optional `--cli <path>` selects another CLI. The standalone desktop executable requires an already-installed service/CLI for that action.

For installation, run `bin/wrongmemory.exe install --desktop` from the extracted bundle, or use `irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install-desktop.ps1 | iex`. The per-user installer verifies the archive and all manifest files, installs all three binaries plus license/source materials, and does not launch a window. Repeat to update; `wrongmemory uninstall` removes owned program files and keeps knowledge by default.

`wrongmemory-desktop.exe --data-root <directory>` selects a service root. Development and evaluation must use disposable roots; adopting a live SAGE store is a separately requested operation with backup. The desktop connects using private local authenticated IPC; no token belongs in arguments or configuration.

The library offers paged search, status filters and a virtualized list. Select a record to inspect full text and evidence. Editing uses revision checks. If a concurrent writer changes the record, **Compare current revision** shows server text while retaining the draft; **Use revision N for next save** explicitly adopts the new base. It does not automatically merge text or provide historical text recovery. Review, bounded relations, grant management and service health use the same daemon.

Ctrl+F focuses search, Ctrl+K / Ctrl+P opens the palette, Ctrl+R refreshes. List arrows select records. Relations support zoom and middle-button panning. Destructive confirmation requires `CONFIRM`. Creating a grant produces a secret-free generic MCP configuration; copying it does not install or certify an external client.

Presentation preferences are stored privately in `<data-root>/desktop-ui.json`: appearance, selected project/view/status, graph zoom and panel widths. Memory text, queries, drafts and credentials are not persisted there. Malformed, oversized or newer-version settings are reported and preserved; move the file aside deliberately to restore defaults. Preference writes are asynchronous; an immediate process exit can lose the latest change. Unsaved drafts exist only in the running window.

## Build and verify

The independent workspace pins Rust 1.99.1 and GPUI Kit 0.9.1. The core remains Rust 1.93.0. Windows requires MSVC, Windows SDK and CMake. From the repository root, build core fixtures with `cargo build --locked -p wrongmemory-cli -p wrongmemoryd`. Then from `apps/desktop` run:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```

Set `WRONGMEMORY_TEST_CLI` to a built core CLI when it is not in the repository's default debug output. Tests spawn real disposable daemons and fail if the binary is missing. Lifecycle/client changes require five consecutive daemon integration runs. If an older debug desktop is open, use `--target-dir ../../target/desktop-verify` for checks to avoid replacing it.

From the root, `bun run desktop:package` builds the release executables and creates a Windows ZIP, bare GUI executable, SHA256SUMS, manifest and dependency license inventory/texts in `dist-desktop`. Use `--out <fresh directory>` for another run; existing packages are never overwritten. `--skip-build` packages existing release binaries. `bun run desktop:package:test` verifies archive structure and overwrite refusal.

Packages are unsigned previews. Local builds are not hosted CI or published release evidence. Initial Windows library/relations rendering has been observed on a disposable fixture; current conflict/settings controls, complete keyboard/screen-reader/IME behavior, 100/150/200% DPI, GPU fallback, sleep/resume and sustained UI performance still require runtime acceptance. Linux/macOS native builds and launches are NOT RUN. The browser dashboard and headless service remain available.

`bun scripts/desktop/verify.ts --out <package directory> --runtime` independently extracts the ZIP, validates its checksums and manifest, compares the bare GUI, and checks all three executable version banners without opening a GUI window. `bun scripts/desktop/install-smoke.ts --out <package directory> --bundle <extracted directory>` exercises the public installer against a local mirror and disposable prefix/root: corrupt download refusal, fresh install, live-daemon update, record preservation and uninstall/autostart cleanup.

`THIRD_PARTY.json` records exact dependency versions, authors, declarations and source archives. Omitted upstream workspace license texts are included from the crate's recorded VCS commit with hashes/URLs in `third-party-licenses/SOURCES.json`. Three crates publish declarations without an upstream license file; their registry declarations, standard license text and checksum-verified corresponding source are bundled and explicitly labeled. Corresponding source for the MPL-2.0 dependency is bundled too. New dependencies without license material fail packaging.
