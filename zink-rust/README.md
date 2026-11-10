# Zink Rust migration

This directory is the side-by-side native Rust migration of Zink. The original
C#/WinUI project remains authoritative until each capability has passed its
acceptance gate in `docs/MIGRATION_INVENTORY.md`.

## Prerequisites

- Windows 10 1809 or later, or Windows 11
- Visual Studio 2022 Build Tools with Desktop development with C++ and a Windows SDK
- Rust 1.92 (selected by `rust-toolchain.toml`)

## Development build

```powershell
cd zink-rust
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p zink-app
```

The application shell, navigation, durable settings, logging, Windows lifecycle
boundary, native file picker, and first local playback controls compile and run.
Feature status is documented honestly in the migration inventory; an item is
not complete merely because its route is visible in the shell.

Zink Call uses Media Foundation for hardware video encoding and rav1d for
720p AV1 decoding. Rav1d is statically linked under the BSD 2-Clause license.
Include [its license notice](docs/LICENSE-rav1d.txt) when distributing Zink.
Shared PC audio uses WASAPI process loopback, which requires Windows build
20348 or newer and excludes audio played by Zink and its child processes.
