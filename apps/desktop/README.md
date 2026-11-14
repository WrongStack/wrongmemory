# WrongMemory Desktop preview

A native Rust / GPUI Kit knowledge workbench for the existing WrongMemory service. One code base targets Windows x64, Linux x64 and macOS arm64. This optional client does not own SQLite, start a daemon automatically, install itself, or stop the daemon when its window closes. Each machine runs one per-user daemon; the desktop, WrongStack and MCP clients share it.

## Run

Unpack the complete desktop archive (ZIP on Windows, tar.gz on Linux and macOS) and run `bin/wrongmemory-desktop` (`.exe` on Windows). Keep the adjacent `wrongmemory.exe` and `wrongmemoryd.exe`: the explicit **Start service** button delegates to the existing CLI. An optional `--cli <path>` selects another CLI. The standalone desktop executable requires an already-installed service/CLI for that action.

For installation, run `bin/wrongmemory install --desktop` from the extracted bundle, or use `irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install-desktop.ps1 | iex` on Windows and `curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.sh | WRONGMEMORY_DESKTOP=1 sh` on Linux and macOS (from the first release that publishes those bundles). The per-user installer verifies the archive and all manifest files, installs all three binaries plus license/source materials, and does not launch a window. Repeat to update; `wrongmemory uninstall` removes owned program files and keeps knowledge by default.

`wrongmemory-desktop.exe --data-root <directory>` selects a service root. Development and evaluation must use disposable roots; adopting a live SAGE store is a separately requested operation with backup. The desktop connects using private local authenticated IPC; no token belongs in arguments or configuration.

The library offers paged search, status filters and a virtualized list. Select a record to inspect full text and evidence. Editing uses revision checks. If a concurrent writer changes the record, **Compare current revision** shows server text while retaining the draft; **Use revision N for next save** explicitly adopts the new base. It does not automatically merge text or provide historical text recovery. Review, bounded relations, grant management and service health use the same daemon.

Ctrl+F focuses search, Ctrl+K / Ctrl+P opens the command palette (views, actions, projects), Ctrl+R refreshes. On macOS these use Cmd, and Cmd+Q quits from the application menu. List arrows select records. Relations support zoom and middle-button panning. Destructive confirmation requires `CONFIRM`. Creating a grant produces a secret-free generic MCP configuration; copying it does not install or certify an external client.

Presentation preferences are stored privately in `<data-root>/desktop-ui.json`: appearance, selected project/view/status, graph zoom and panel widths. Memory text, queries, drafts and credentials are not persisted there. Malformed, oversized or newer-version settings are reported and preserved; move the file aside deliberately to restore defaults. Preference writes are asynchronous; an immediate process exit can lose the latest change. Unsaved drafts exist only in the running window.

## Build and verify

The independent workspace pins Rust 1.99.1 and GPUI Kit 0.9.1. The core remains Rust 1.93.0. Windows requires MSVC, Windows SDK and CMake. macOS requires the Xcode command-line tools. Debian/Ubuntu need `clang cmake pkg-config libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev libx11-xcb-dev libxcb1-dev libfontconfig1-dev libfreetype6-dev libzstd-dev libssl-dev`, plus at runtime `libxcb1`, `libxkbcommon0`, `libxkbcommon-x11-0` (linked) and a Vulkan loader and driver (`libvulkan1`, Mesa is enough), fontconfig and, for Wayland sessions, `libwayland-client0` (loaded on demand). From the repository root, build core fixtures with `cargo build --locked -p wrongmemory-cli -p wrongmemoryd`. Then from `apps/desktop` run:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```

Set `WRONGMEMORY_TEST_CLI` to a built core CLI when it is not in the repository's default debug output. Tests spawn real disposable daemons and fail if the binary is missing. Lifecycle/client changes require five consecutive daemon integration runs. If an older debug desktop is open, use `--target-dir ../../target/desktop-verify` for checks to avoid replacing it.

From the root, `bun run desktop:package` builds the release executables and creates, for the host platform, a ZIP (Windows) or tar.gz (Linux, macOS), the bare GUI executable, SHA256SUMS, manifest and dependency license inventory/texts in `dist-desktop`. Use `--out <fresh directory>` for another run; existing packages are never overwritten. `--skip-build` packages existing release binaries. `bun run desktop:package:test` verifies archive structure and overwrite refusal.

Packages are unsigned previews. Local builds are not hosted CI or published release evidence. All five views, light/dark themes, the command palette and the disconnected/start-service flow were checked on Windows at 100% scaling against a disposable fixture, and the library on Linux under WSLg (X11). Complete keyboard/screen-reader/IME behavior, 150/200% DPI, GPU fallback, sleep/resume, native Wayland rendering, macOS rendering and sustained UI performance still require runtime acceptance. The browser dashboard and headless service remain available.

`bun scripts/desktop/verify.ts --out <package directory> --runtime` independently extracts the archive, validates its checksums and manifest, compares the bare GUI, and checks all three executable version banners without opening a GUI window. `bun scripts/desktop/install-smoke.ts --out <package directory> --bundle <extracted directory>` exercises the public installer against a local mirror and disposable prefix/root: corrupt download refusal, fresh install, live-daemon update, record preservation and uninstall/autostart cleanup.

`bun scripts/desktop/collect-licenses.ts` refreshes the supplementary license material for the union of the Windows, Linux and macOS dependency graphs. `THIRD_PARTY.json` records exact dependency versions, authors, declarations and source archives. Omitted upstream workspace license texts are included from the crate's recorded VCS commit with hashes/URLs in `third-party-licenses/SOURCES.json`. Four crates (mac, pathfinder_geometry, seahash, simd_helpers) publish declarations without an upstream license file; their registry declarations, standard license text and checksum-verified corresponding source are bundled and explicitly labeled. Corresponding source for the MPL-2.0 dependency is bundled too. New dependencies without license material fail packaging.
