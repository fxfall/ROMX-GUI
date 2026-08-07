# ROMX GUI

Cross-platform ROMX desktop application workspace.

## Workspace

- `crates/romx-core`: Rust implementation of ROMX 1.0 binary packing, footer parsing, validation, SHA-256 checks, PNG cover checks, and extraction.
- Future Tauri frontend: metadata forms, LPL import/export, batch progress, and network metadata providers.

## Core API

The core exposes byte-oriented functions for GUI and CLI callers:

- `pack_bytes` / `pack_to_path`: create a ROMX container from an unchanged ROM, JSON metadata, and optional PNG bytes.
- `read_bytes` / `read_path`: parse and validate footer, regions, metadata, cover, and hashes.
- `extract_to_dir`: write the embedded payload, metadata, and cover to a directory.
- `required_metadata`: create the four required metadata fields for a GUI form.

The core does not start an emulator and does not contain UI code. LPL integration will be added above this format layer so the same binary implementation is shared by the GUI and future CLI.

## Local development

Install Rust from <https://www.rust-lang.org/tools/install>, then run:

```bash
cargo fmt --all
cargo test --workspace
```
