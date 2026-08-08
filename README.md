# ROMX GUI

Cross-platform ROMX desktop application workspace.

## ROMX GUI

The ROMX GUI is a cross-platform Slint + Rust desktop application for packing
and unpacking ROMX files. It calls `romx-core` directly and targets macOS,
Linux, and Windows.

Run the GUI from source with:

```bash
cargo run -p romx-gui
```

When macOS reports that a downloaded or extracted `romx-gui.app` is damaged,
remove the quarantine attribute from this trusted local build, then open it:

```bash
xattr -cr "./romx-gui.app"
open "./romx-gui.app"
```

The packaged macOS layout is `romx-gui.app`; Linux packages contain one
`romx-gui` executable and Windows packages contain one `romx-gui.exe`.

The current GUI provides:

- top-level packing and unpacking pages;
- unified left navigation for packing/unpacking and their game-file/LPL sub-pages;
- game-file packing with native file/folder dialogs;
- PNG/JPEG/WebP/GIF/BMP cover preview and exact PNG resizing;
- metadata JSON import plus the requested custom game-information fields;
- direct packing through `romx-core`, with the generated ROMX path shown in the status bar;
- ROMX verification and extraction;
- simplified Chinese/English switching and portable minimize/maximize/close controls.

The GUI uses process-local temporary playlists with English names such as
`temp-single-01.lpl`, `temp-list-01.lpl`, and `temp-list-01-2.lpl`. Single-file
packing and LPL conversion both pass an LPL directly to `romx-core`. During a
single-item edit, `temp-list-01-2.lpl` is converted independently; Save merges
it into `temp-list-01.lpl`, while Back removes the edit playlist. The temporary
workspace is removed when a playlist is replaced or the GUI exits.

### GUI locales

GUI text is stored outside the source in `crates/romx-gui/locales/en.json` and
`crates/romx-gui/locales/zh-CN.json`. At runtime the loader checks
`ROMX_LOCALE_DIR`, then a `locales` directory beside the executable (or
`Contents/Resources/locales` inside the macOS app), and finally the development
locale directory. To add another language, copy `en.json`, translate its
values, and add the language to the Slint language selector.

## Workspace

- `crates/romx-core`: Rust implementation of ROMX 1.0 binary packing, footer parsing, validation, SHA-256 checks, PNG cover checks, and extraction.
- `crates/romx-cli`: command-line interface for Core file operations and LPL import/export.
- `crates/romx-gui`: Slint + Rust desktop GUI. It calls `romx-core` directly and is intended to target macOS, Windows, and Linux.

The current release line is `0.2`, represented as SemVer `0.2.1`. The single
version source is `[workspace.package]` in the root `Cargo.toml`; all crates
inherit it. Rust callers can read `romx_core::APP_VERSION` or call
`romx_core::application_version()`, and the CLI exposes the same value through
`romx --version`.

## Core API

The core exposes byte-oriented functions for GUI and CLI callers:

- `pack_bytes` / `pack_to_path`: create a ROMX container from an unchanged ROM, JSON metadata, and an optional PNG/JPG/JPEG/WebP/GIF/BMP cover. PNG bytes are preserved by default; other formats are normalized to PNG. They always regenerate metadata `crc32` from the original ROM.
- `pack_bytes_with_crc32` / `pack_to_path_with_crc32`: the same operations with an explicit eight-digit CRC32 lookup override; footer SHA-256 still covers the actual ROM.
- `normalize_cover_bytes` / `normalize_cover_path`: accept PNG, JPG/JPEG, WebP, GIF, and BMP; preserve PNG bytes by default, or convert/resize any supported format to an exact PNG resolution.
- `read_bytes` / `read_path`: parse and validate footer, regions, metadata, cover, and hashes.
- `read_metadata_cover_bytes` / `read_metadata_cover_path`: lightweight preview readers that load only the footer, metadata, and optional cover; they intentionally skip the ROM payload and payload/body hash checks.
- `extract_to_dir`: write the embedded payload, metadata, and cover to a directory.
- `required_metadata`: create the four required metadata fields for a GUI form.
- `classify_gb_payload`: apply the `0xC0`/`0x80` Game Boy CGB-flag policy.
- `crc32` / `normalize_crc32`: calculate or validate the RetroArch/database lookup key; footer SHA-256 remains the integrity check.
- `plan_lpl_import`: resolve every LPL item to its ROM and optional thumbnail without copying large payloads; intended for GUI import previews.
- `import_lpl`: convert an LPL and its ROM/thumbnail tree to sequential ROMX files plus a manifest. With no roots, real absolute ROM paths and RetroArch virtual `/roms/...` paths are resolved from the LPL location, and the sibling thumbnail tree is inferred; useful LPL fields become metadata and frontend-only fields are preserved under `x-retroarch`.
- `export_lpl`: extract a ROMX directory to a RetroArch-compatible ROM, thumbnail, and playlist tree.

The core does not start an emulator and does not contain UI code. LPL integration will be added above this format layer so the same binary implementation is shared by the GUI and future CLI.

## Local development

Install Rust from <https://www.rust-lang.org/tools/install>, then run:

```bash
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The repository includes `rust-toolchain.toml`, so rustup automatically selects
the current stable toolchain and the `rustfmt`/`clippy` components on macOS,
Linux, and Windows. The GUI uses Slint's software renderer with winit, so no
platform-specific Rust feature flags are needed.

On Debian-based Linux systems, install the native GUI development packages
before building `romx-gui`:

```bash
sudo apt-get update
sudo apt-get install -y \
  pkg-config libfontconfig1-dev libfreetype-dev \
  libx11-dev libx11-xcb-dev libxcb1-dev libxcb-render0-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libxcb-xkb-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libegl1-mesa-dev libudev-dev
```

Then build the Linux GUI with:

```bash
CARGO_TARGET_DIR=target/linux cargo build -p romx-gui --release
./target/linux/release/romx-gui
```

Using a separate target directory is recommended when the Linux workspace is
mounted from macOS through OrbStack, so Linux artifacts do not replace the
native macOS artifacts in `target/release`.

### GitHub packages

Pushing a `v*` version tag or manually running `.github/workflows/build.yml`
builds and tests native arm64 and x86_64 packages for macOS, Linux, and Windows.
The GUI release
layouts are platform-native: macOS archives contain `romx-gui.app`, Linux
archives contain the single `romx-gui` executable, and Windows archives contain
the single `romx-gui.exe`. Pushing a tag such as `v0.2.1` additionally attaches
the archives and SHA-256 files to the GitHub release. A manual workflow run
creates downloadable Actions artifacts without publishing a release.

The compatibility test requires the optional reference data directory and is
ignored in portable CI. When that data is available, run it explicitly:

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
# Convert a non-PNG cover, or resize any cover, to an exact PNG resolution.
./target/release/romx pack game.gba metadata.json --cover cover.webp --cover-size 320x320 --output game.gbax
# Optional database identity override; otherwise CRC32 is regenerated from game.gba.
./target/release/romx pack game.gba metadata.json --crc32 0123abcd --output game.gbax
./target/release/romx inspect game.gbax
./target/release/romx verify game.gbax
./target/release/romx extract game.gbax --output extracted
```

Import and export RetroArch playlists:

```bash
./target/release/romx import-lpl \
  ./data/retroarch/playlists/00-GB.lpl \
  --rom-root ./data \
  --cover-root ./data/retroarch/thumbnails \
  --output ./build/romx/00-GB

# If the LPL contains absolute ROM paths, no ROM/cover roots are required.
./target/release/romx import-lpl ./data/playlist.lpl \
  --output ./build/romx-out

# Optional: force one database CRC32 identity for all imported entries.
./target/release/romx import-lpl ./data/playlist.lpl --rom-dir ./data/roms \
  --crc32 0123abcd --output ./build/romx-out

./target/release/romx export-lpl \
  ./build/romx/00-GB \
  --output ./build/retroarch
```
