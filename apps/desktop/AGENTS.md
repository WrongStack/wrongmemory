# Native desktop workspace

This is an independent optional workspace (WM-ADR-014). Run Cargo from this directory to select Rust 1.99.1. The core workspace stays on Rust 1.93.0. New desktop dependencies are pinned exactly in this workspace's Cargo.toml.

Use only authenticated IPC. Never open a project database, show credentials, accept arbitrary IPC methods, or shut down the daemon when the window closes. Preserve the root AGENTS.md invariants.

Required checks:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --locked
```

Real-daemon tests require WRONGMEMORY_TEST_CLI pointing to a built core CLI. CI must set it and must not silently accept missing fixture binaries. GUI runtime checks use disposable data roots only. Record native rendering and platform/accessibility checks separately from compile and IPC tests, per platform (Windows, Linux X11, Linux Wayland, macOS).

The same source targets Windows x64, Linux x64 and macOS arm64. Shortcuts go through `ui::binding` (Cmd on macOS). Any `IconName` outside GPUI Kit's default embedded set must be listed in `ui::ExtraIcons`; the `every_referenced_icon_is_embedded` test enforces it. After adding or updating a dependency, run `bun scripts/desktop/collect-licenses.ts` so every platform's package has license material.
