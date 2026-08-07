//! Compatibility test for the data tree used by the Python reference tool.
//!
//! This is ignored during normal CI because the fixture tree is local and is
//! several gigabytes. Run it explicitly with:
//! `cargo test -p romx-core --test real_data_compat -- --ignored --nocapture`

use romx_core::{
    crc32, import_lpl, plan_lpl_import, read_path, required_metadata, ImportLplOptions,
};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

const DATA_ROOT: &str = "/Volumes/DATA/rom";
const PYTHON_REFERENCE: &str = "/Volumes/DATA/romx/tools/romx.py";
const PLAYLISTS: &[(&str, usize)] = &[
    ("00-GB.lpl", 110),
    ("01-GBC.lpl", 165),
    ("02-GBA.lpl", 459),
    ("PKM.lpl", 90),
];

#[test]
#[ignore = "requires the local /Volumes/DATA/rom reference data"]
fn python_reference_directories_resolve_and_roundtrip() {
    let data_root = Path::new(DATA_ROOT);
    let playlist_root = data_root.join("retroarch/playlists");
    let options = ImportLplOptions {
        rom_root: Some(data_root.to_owned()),
        cover_root: Some(data_root.join("retroarch/thumbnails")),
        ..Default::default()
    };
    let temporary = tempdir().unwrap();
    let mut total_items = 0;
    let mut total_covers = 0;

    for (playlist_file, expected_items) in PLAYLISTS {
        let source_lpl = playlist_root.join(playlist_file);
        let plan = plan_lpl_import(&source_lpl, &options).unwrap();
        assert_eq!(plan.total_items, *expected_items, "{playlist_file}");
        assert_eq!(plan.items.len(), *expected_items, "{playlist_file}");
        assert_eq!(plan.skipped, 0, "{playlist_file}");
        total_items += plan.items.len();
        total_covers += plan
            .items
            .iter()
            .filter(|item| item.cover_path.is_some())
            .count();

        // Import one real entry from each unmodified playlist, preserving its
        // playlist filename so platform and thumbnail lookup remain identical.
        let mut document: Value = serde_json::from_slice(&fs::read(&source_lpl).unwrap()).unwrap();
        let first_item = document["items"][0].clone();
        document["items"] = Value::Array(vec![first_item]);
        let sample_lpl = temporary.path().join(playlist_file);
        fs::write(&sample_lpl, serde_json::to_vec(&document).unwrap()).unwrap();
        let output_dir = temporary.path().join(format!("{playlist_file}.romx"));
        let report = import_lpl(&sample_lpl, &output_dir, &options).unwrap();
        assert_eq!(report.imported, 1);
        let python_verify = Command::new("python3")
            .arg(PYTHON_REFERENCE)
            .arg("verify")
            .arg(&report.output_files[0])
            .status()
            .unwrap();
        assert!(
            python_verify.success(),
            "Python rejected Rust output for {playlist_file}"
        );
        let packed = read_path(&report.output_files[0]).unwrap();
        assert_eq!(packed.rom, fs::read(&plan.items[0].rom_path).unwrap());
        assert_eq!(
            packed.metadata.as_ref().unwrap()["label"],
            plan.items[0].label
        );
        assert_eq!(packed.cover.is_some(), plan.items[0].cover_path.is_some());
        if let Some(cover_path) = &plan.items[0].cover_path {
            assert_eq!(
                packed.cover.as_deref(),
                Some(fs::read(cover_path).unwrap().as_slice())
            );
        }

        let metadata_path = temporary
            .path()
            .join(format!("{playlist_file}.metadata.json"));
        let metadata = required_metadata(
            &plan.items[0].label,
            &plan.items[0].platform,
            &plan.items[0].payload_format,
        );
        fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let python_output = temporary
            .path()
            .join(format!("{playlist_file}.python.romx"));
        let mut python_pack = Command::new("python3");
        python_pack
            .arg(PYTHON_REFERENCE)
            .arg("pack")
            .arg(&plan.items[0].rom_path)
            .arg(&metadata_path)
            .arg("--output")
            .arg(&python_output);
        if let Some(cover_path) = &plan.items[0].cover_path {
            python_pack.arg("--cover").arg(cover_path);
        }
        assert!(
            python_pack.status().unwrap().success(),
            "Python failed to pack {playlist_file}"
        );
        let python_packed = read_path(&python_output).unwrap();
        assert_eq!(python_packed.rom, packed.rom);
        let mut expected_metadata = metadata;
        expected_metadata["crc32"] = Value::String(crc32(&python_packed.rom));
        assert_eq!(python_packed.metadata, Some(expected_metadata));
        assert_eq!(python_packed.cover, packed.cover);
    }

    assert_eq!(total_items, 824);
    println!("validated {total_items} LPL items and found {total_covers} matching PNG covers");
}
