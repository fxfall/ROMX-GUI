//! Optional integration test against the local ROM collection and the
//! independent ROMX 0.2.0 Python reference inspector.

use romx_core::{import_lpl, plan_lpl_import, read_path, ImportLplOptions};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

const DATA_ROOT: &str = "/Volumes/rom";
const PYTHON_REFERENCE: &str = "/Volumes/Repositories/romx/tools/romx_reference.py";

#[test]
#[ignore = "requires the local /Volumes/rom reference data"]
fn real_lpl_entries_are_accepted_by_the_python_reference() {
    let data_root = Path::new(DATA_ROOT);
    let source_lpl = data_root.join("retroarch/playlists/FC.lpl");
    let options = ImportLplOptions {
        rom_root: Some(data_root.to_owned()),
        cover_root: Some(data_root.join("retroarch/thumbnails")),
        skip_missing: false,
        ..Default::default()
    };
    let plan = plan_lpl_import(&source_lpl, &options).unwrap();
    assert!(!plan.items.is_empty());

    let temporary = tempdir().unwrap();
    let mut document: Value = serde_json::from_slice(&fs::read(&source_lpl).unwrap()).unwrap();
    document["items"] = Value::Array(vec![document["items"][0].clone()]);
    let sample_lpl = temporary.path().join("FC.lpl");
    fs::write(&sample_lpl, serde_json::to_vec(&document).unwrap()).unwrap();
    let output_dir = temporary.path().join("romx");
    let report = import_lpl(&sample_lpl, &output_dir, &options).unwrap();
    assert_eq!(report.imported, 1);
    let packed = read_path(&report.output_files[0]).unwrap();
    assert_eq!(packed.rom, fs::read(&plan.items[0].rom_path).unwrap());
    assert_eq!(packed.footer.version, 2);

    let status = Command::new("python3")
        .arg(PYTHON_REFERENCE)
        .arg("inspect")
        .arg(&report.output_files[0])
        .arg("--verify-entry-crc32")
        .status()
        .unwrap();
    assert!(status.success(), "Python reference rejected Rust output");
}
