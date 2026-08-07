use romx_core::{
    export_lpl, import_lpl, plan_lpl_import, read_path, ExportLplOptions, ImportLplOptions,
};
use serde_json::{json, Value};
use std::fs;
use tempfile::tempdir;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x10\0\0\0\x20";

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
            "items": [{"path": "/roms/02-GBA/game.gba", "label": "中文/Game"}]
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
    assert_eq!(document.metadata.as_ref().unwrap()["label"], "中文/Game");
    assert_eq!(document.metadata.as_ref().unwrap()["crc32"], "6b8a1dc0");
    assert_eq!(document.metadata.as_ref().unwrap()["cover"]["width"], 16);
    assert_eq!(document.metadata.as_ref().unwrap()["cover"]["height"], 32);
    assert_eq!(document.cover.as_deref(), Some(PNG));

    let export_root = root.path().join("export");
    let exported = export_lpl(&romx_dir, &export_root, &ExportLplOptions::default()).unwrap();
    assert_eq!(exported.exported, 1);
    assert_eq!(
        fs::read(exported.rom_dir.join("000001.gba")).unwrap(),
        b"real-rom-bytes"
    );
    assert_eq!(
        fs::read(exported.cover_dir.join("中文_Game.png")).unwrap(),
        PNG
    );
    let lpl: Value = serde_json::from_slice(&fs::read(exported.lpl_path).unwrap()).unwrap();
    assert_eq!(lpl["items"][0]["path"], "/roms/02-GBA/000001.gba");
    assert_eq!(lpl["items"][0]["label"], "中文/Game");
    assert_eq!(lpl["items"][0]["crc32"], "6b8a1dc0|crc");
}

#[test]
fn plan_can_skip_missing_entries_without_renumbering_outputs() {
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
    assert!(output.join("000002.gbx").is_file());
}
