# ROMX GUI

Cross-platform ROMX desktop application workspace.

## Workspace

- `crates/romx-core`: Rust implementation of ROMX 1.0 binary packing, footer parsing, validation, SHA-256 checks, PNG cover checks, and extraction.
- `crates/romx-cli`: command-line interface for Core file operations and LPL import/export.
- Future Tauri frontend: metadata forms, LPL import/export, batch progress, and network metadata providers.

## Core API

The core exposes byte-oriented functions for GUI and CLI callers:

- `pack_bytes` / `pack_to_path`: create a ROMX container from an unchanged ROM, JSON metadata, and optional PNG bytes.
- `read_bytes` / `read_path`: parse and validate footer, regions, metadata, cover, and hashes.
- `extract_to_dir`: write the embedded payload, metadata, and cover to a directory.
- `required_metadata`: create the four required metadata fields for a GUI form.
- `classify_gb_payload`: apply the `0xC0`/`0x80` Game Boy CGB-flag policy.
- `crc32`: calculate the RetroArch/database lookup key; footer SHA-256 remains the integrity check.
- `plan_lpl_import`: resolve every LPL item to its ROM and optional thumbnail without copying large payloads; intended for GUI import previews.
- `import_lpl`: convert an LPL and its ROM/thumbnail tree to sequential ROMX files plus a manifest.
- `export_lpl`: extract a ROMX directory to a RetroArch-compatible ROM, thumbnail, and playlist tree.

The core does not start an emulator and does not contain UI code. LPL integration will be added above this format layer so the same binary implementation is shared by the GUI and future CLI.

## Local development

Install Rust from <https://www.rust-lang.org/tools/install>, then run:

```bash
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The compatibility test uses the same local paths as the Python reference tool
and is ignored in portable CI. On the reference machine, run it explicitly:

```bash
cargo test -p romx-core --test real_data_compat -- --ignored --nocapture
```

## CLI

Build the release executable:

```bash
cargo build -p romx-cli --release
./target/release/romx --help
```

Pack, inspect, verify, and extract a ROMX file:

```bash
./target/release/romx pack game.gba metadata.json --cover cover.png --output game.gbax
./target/release/romx inspect game.gbax
./target/release/romx verify game.gbax
./target/release/romx extract game.gbax --output extracted
```

Import and export RetroArch playlists:

```bash
./target/release/romx import-lpl \
  /Volumes/DATA/rom/retroarch/playlists/00-GB.lpl \
  --rom-root /Volumes/DATA/rom \
  --cover-root /Volumes/DATA/rom/retroarch/thumbnails \
  --output /Volumes/DATA/romx-gui/test-output/00-GB

./target/release/romx export-lpl \
  /Volumes/DATA/romx-gui/test-output/00-GB \
  --output /Volumes/DATA/romx-gui/test-retroarch
```
