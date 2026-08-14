use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
use romx_core::{
    export_lpl, import_lpl, import_lpl_with_output_handling, plan_lpl_import, read_path,
    ExportLplOptions, ImportLplOptions,
};
use serde_json::{json, Value};
use std::fs;
use std::io::Cursor;
use tempfile::tempdir;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x00, 0x02, 0x00,
    0x00, 0x05, 0x00, 0x01, 0x7a, 0x5e, 0xab, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

#[test]
fn lpl_import_and_export_preserve_payload_metadata_and_cover() {
    let root = tempdir().unwrap();
    let rom_root = root.path().join("source");
    let cover_root = root.path().join("thumbnails");
    fs::create_dir_all(rom_root.join("roms/02-GBA")).unwrap();
    fs::create_dir_all(cover_root.join("02-GBA/Named_Snaps")).unwrap();
    fs::write(rom_root.join("roms/02-GBA/game.gba"), b"real-rom-bytes").unwrap();
    fs::write(cover_root.join("02-GBA/Named_Snaps/game.png"), PNG).unwrap();
    let lpl_path = root.path().join("02-GBA.lpl");
    fs::write(
        &lpl_path,
        serde_json::to_vec(&json!({
            "version": "1.5",
            "items": [{
                "path": "/roms/02-GBA/game.gba",
                "label": "中文/Game",
                "core_name": "FBNeo",
                "crc32": "DEADBEEF|crc",
                "db_name": "Nintendo - Game Boy Advance.lpl"
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let options = ImportLplOptions {
        rom_root: Some(rom_root),
        cover_root: Some(cover_root),
        ..Default::default()
    };

    let plan = plan_lpl_import(&lpl_path, &options).unwrap();
    assert_eq!(plan.total_items, 1);
    assert_eq!(plan.items[0].platform, "gba");
    assert!(plan.items[0].cover_path.is_some());

    let romx_dir = root.path().join("romx");
    let imported = import_lpl(&lpl_path, &romx_dir, &options).unwrap();
    assert_eq!(imported.imported, 1);
    let document = read_path(&imported.output_files[0]).unwrap();
    assert_eq!(document.rom, b"real-rom-bytes");
    assert_eq!(document.metadata.as_ref().unwrap()["name"], "中文/Game");
    assert_eq!(document.metadata.as_ref().unwrap()["crc32"], "6b8a1dc0");
    assert!(document
        .metadata
        .as_ref()
        .unwrap()
        .get("x-retroarch")
        .is_none());
    assert_eq!(document.metadata.as_ref().unwrap()["cover"]["width"], 1);
    assert_eq!(document.metadata.as_ref().unwrap()["cover"]["height"], 1);
    assert_eq!(document.cover.as_deref(), Some(PNG));

    let export_root = root.path().join("export");
    let exported = export_lpl(&romx_dir, &export_root, &ExportLplOptions::default()).unwrap();
    assert_eq!(exported.exported, 1);
    assert_eq!(
        fs::read(exported.rom_dir.join("game.gba")).unwrap(),
        b"real-rom-bytes"
    );
    assert_eq!(fs::read(exported.cover_dir.join("game.png")).unwrap(), PNG);
    let lpl: Value = serde_json::from_slice(&fs::read(exported.lpl_path).unwrap()).unwrap();
    assert_eq!(lpl["items"][0]["path"], "/roms/02-GBA/game.gba");
    assert_eq!(lpl["items"][0]["label"], "中文/Game");
    assert_eq!(lpl["items"][0]["crc32"], "6b8a1dc0|crc");
    assert_eq!(lpl["items"][0]["core_name"], "FBNeo");
    assert_eq!(
        lpl["items"][0]["db_name"],
        "Nintendo - Game Boy Advance.lpl"
    );

    let single_root = root.path().join("single-export");
    let single = export_lpl(
        &imported.output_files[0],
        &single_root,
        &ExportLplOptions {
            playlist_name: Some("Custom".into()),
            lpl_rom_prefix: Some("/custom/roms".into()),
            lpl_cover_prefix: Some("/custom/covers".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(single.total_items, 1);
    assert_eq!(single.exported, 1);
    assert_eq!(single.skipped, 0);
    assert!(single.rom_dir.join("game.gba").is_file());
    let custom_lpl: Value = serde_json::from_slice(&fs::read(&single.lpl_path).unwrap()).unwrap();
    assert_eq!(custom_lpl["items"][0]["path"], "/custom/roms/game.gba");
    assert!(custom_lpl["items"][0].get("cover_path").is_none());
    assert!(single.cover_dir.join("game.png").is_file());
}

#[test]
fn lplx_carries_romx_metadata_and_core_but_lpl_export_is_official_only() {
    let root = tempdir().unwrap();
    let rom_path = root.path().join("game.gba");
    let cover_dir = root.path().join("covers");
    fs::write(&rom_path, b"lplx-rom").unwrap();
    fs::create_dir_all(&cover_dir).unwrap();
    fs::write(cover_dir.join("game.png"), PNG).unwrap();
    let lplx_path = root.path().join("custom.lplx");
    fs::write(
        &lplx_path,
        serde_json::to_vec(&json!({
            "version": "1.5",
            "scan_content_dir": "/roms",
            "unknown_root": true,
            "items": [{
                "path": rom_path,
                "label": "Playlist Label",
                "core_path": "/cores/gba_libretro.dylib",
                "core_name": "GBA Core",
                "db_name": "Nintendo - Game Boy Advance.lpl",
                "cover_path": cover_dir,
                "metadata": {
                    "developer": "ROMX Developer",
                    "genre": ["RPG"],
                    "cover": {"mime_type": "image/png", "width": 1, "height": 1},
                    "unknown_item": "discard me"
                }
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let romx_dir = root.path().join("romx");
    let imported = import_lpl(&lplx_path, &romx_dir, &ImportLplOptions::default()).unwrap();
    let document = read_path(&imported.output_files[0]).unwrap();
    let metadata = document.metadata.as_ref().unwrap();
    assert_eq!(metadata["name"], "Playlist Label");
    assert_eq!(document.footer.platform_id, 3);
    assert_eq!(document.entries[0].format_id, 3);
    assert_eq!(metadata["developer"], "ROMX Developer");
    assert_eq!(metadata["genre"], json!(["RPG"]));
    assert_eq!(metadata["cover"]["mime_type"], "image/png");
    assert_eq!(document.cover.as_deref(), Some(PNG));
    assert!(metadata.get("unknown_item").is_none());

    let exported = export_lpl(&romx_dir, &root.path().join("export"), &Default::default()).unwrap();
    let lpl: Value = serde_json::from_slice(&fs::read(exported.lpl_path).unwrap()).unwrap();
    let root_object = lpl.as_object().unwrap();
    assert_eq!(root_object["scan_content_dir"], "/roms");
    assert!(root_object.get("unknown_root").is_none());
    let item = lpl["items"][0].as_object().unwrap();
    assert_eq!(item["core_path"], "/cores/gba_libretro.dylib");
    assert_eq!(item["core_name"], "GBA Core");
    assert_eq!(item["db_name"], "Nintendo - Game Boy Advance.lpl");
    assert!(item.get("developer").is_none());
    assert!(item.get("unknown_item").is_none());
    assert!(item.get("cover_path").is_none());
}

#[test]
fn lpl_import_finds_and_normalizes_non_png_covers() {
    let root = tempdir().unwrap();
    let rom_dir = root.path().join("roms");
    let cover_dir = root.path().join("covers");
    fs::create_dir_all(&rom_dir).unwrap();
    fs::create_dir_all(&cover_dir).unwrap();
    fs::write(rom_dir.join("game.gba"), b"rom").unwrap();
    let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(2, 1, Rgb([0, 255, 0])));
    let mut jpeg = Cursor::new(Vec::new());
    image.write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
    fs::write(cover_dir.join("game.jpeg"), jpeg.into_inner()).unwrap();
    let lpl_path = root.path().join("02-GBA.lpl");
    fs::write(
        &lpl_path,
        serde_json::to_vec(&json!({
            "items": [{"path": "/roms/game.gba", "label": "Game"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let output = root.path().join("romx");
    let report = import_lpl(
        &lpl_path,
        &output,
        &ImportLplOptions {
            force_rom_dir: Some(rom_dir),
            force_cover_dir: Some(cover_dir),
            ..Default::default()
        },
    )
    .unwrap();
    let document = read_path(&report.output_files[0]).unwrap();
    assert!(document
        .cover
        .as_ref()
        .unwrap()
        .starts_with(romx_core::PNG_SIGNATURE));
    assert_eq!(document.metadata.as_ref().unwrap()["cover"]["width"], 2);
    assert_eq!(document.metadata.as_ref().unwrap()["cover"]["height"], 1);
}

#[test]
fn plan_can_skip_missing_entries_while_preserving_rom_names() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("present.gb"), b"rom").unwrap();
    let lpl_path = root.path().join("00-GB.lpl");
    fs::write(
        &lpl_path,
        serde_json::to_vec(&json!({"items": [
            {"path": "/missing.gb", "label": "missing"},
            {"path": "/present.gb", "label": "present"}
        ]}))
        .unwrap(),
    )
    .unwrap();
    let options = ImportLplOptions {
        force_rom_dir: Some(root.path().to_owned()),
        skip_missing: true,
        ..Default::default()
    };

    let output = root.path().join("output");
    let report = import_lpl(&lpl_path, &output, &options).unwrap();
    assert_eq!(report.total_items, 2);
    assert_eq!(report.imported, 1);
    assert_eq!(report.skipped, 1);
    assert!(output.join("present.romx").is_file());
}

#[test]
fn temporary_outputs_can_be_committed_one_by_one_with_original_names() {
    let root = tempdir().unwrap();
    let first = root.path().join("Game One.gb");
    let second = root.path().join("Game Two.gb");
    fs::write(&first, b"one").unwrap();
    fs::write(&second, b"two").unwrap();
    let lpl_path = root.path().join("playlist.lpl");
    fs::write(
        &lpl_path,
        serde_json::to_vec(&json!({
            "items": [
                {"path": first, "label": "One"},
                {"path": second, "label": "Two"}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let output = root.path().join("output");
    let mut committed = Vec::new();
    let report = import_lpl_with_output_handling(
        &lpl_path,
        &output,
        &ImportLplOptions {
            temporary_output: true,
            ..Default::default()
        },
        |_current, _total, _imported, _skipped| {},
        || false,
        |_index, _error| false,
        |staged| {
            assert!(staged.is_file());
            assert!(committed
                .iter()
                .all(|path: &std::path::PathBuf| path.is_file()));
            let final_name = staged
                .file_name()
                .unwrap()
                .to_string_lossy()
                .strip_suffix(".tmp")
                .unwrap()
                .to_owned();
            let final_path = staged.with_file_name(final_name);
            fs::rename(staged, &final_path).unwrap();
            committed.push(final_path.clone());
            Ok(Some(final_path))
        },
    )
    .unwrap();
    assert_eq!(report.imported, 2);
    assert_eq!(report.output_files, committed);
    assert!(output.join("Game One.romx").is_file());
    assert!(output.join("Game Two.romx").is_file());
    assert!(!output.join("Game One.romx.tmp").exists());
    assert!(!output.join("Game Two.romx.tmp").exists());
}

#[test]
fn lpl_import_accepts_an_explicit_crc32_override() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("game.gba"), b"real-rom-bytes").unwrap();
    let lpl_path = root.path().join("02-GBA.lpl");
    fs::write(
        &lpl_path,
        serde_json::to_vec(&json!({
            "items": [{"path": "/game.gba", "label": "Game"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let options = ImportLplOptions {
        force_rom_dir: Some(root.path().to_owned()),
        crc32_override: Some("DEADBEEF".into()),
        ..Default::default()
    };
    let output = root.path().join("output");
    let report = import_lpl(&lpl_path, &output, &options).unwrap();
    let document = read_path(&report.output_files[0]).unwrap();
    assert_eq!(document.metadata.as_ref().unwrap()["crc32"], "deadbeef");
}

#[test]
fn standalone_absolute_lpl_paths_resolve_rom_and_retroarch_cover() {
    let root = tempdir().unwrap();
    let retroarch = root.path().join("retroarch");
    let rom_path = root.path().join("roms/game.gba");
    let cover_path = retroarch.join("thumbnails/Absolute/Named_Snaps/game.png");
    fs::create_dir_all(rom_path.parent().unwrap()).unwrap();
    fs::create_dir_all(cover_path.parent().unwrap()).unwrap();
    fs::write(&rom_path, b"absolute-rom").unwrap();
    fs::write(&cover_path, PNG).unwrap();
    let lpl_path = retroarch.join("playlists/Absolute.lpl");
    fs::create_dir_all(lpl_path.parent().unwrap()).unwrap();
    fs::write(
        &lpl_path,
        serde_json::to_vec(&json!({
            "version": "1.5",
            "items": [{"path": rom_path, "label": "game"}]
        }))
        .unwrap(),
    )
    .unwrap();

    let options = ImportLplOptions::default();
    let plan = plan_lpl_import(&lpl_path, &options).unwrap();
    assert_eq!(plan.items[0].rom_path, rom_path);
    assert_eq!(plan.items[0].cover_path, Some(cover_path));
    let report = import_lpl(&lpl_path, &root.path().join("romx"), &options).unwrap();
    let document = read_path(&report.output_files[0]).unwrap();
    assert_eq!(document.cover.as_deref(), Some(PNG));
}
