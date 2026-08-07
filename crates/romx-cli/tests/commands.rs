use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x10\0\0\0\x20";

fn romx(args: &[&Path], literals: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_romx"));
    for literal in literals {
        command.arg(literal);
    }
    for path in args {
        command.arg(path);
    }
    command.output().unwrap()
}

#[test]
fn pack_inspect_verify_and_extract_commands_work() {
    let root = tempdir().unwrap();
    let rom = root.path().join("game.gba");
    let metadata = root.path().join("metadata.json");
    let cover = root.path().join("cover.png");
    let packed = root.path().join("game.gbax");
    let extracted = root.path().join("extracted");
    fs::write(&rom, b"rom-bytes").unwrap();
    fs::write(
        &metadata,
        serde_json::to_vec(&json!({
            "schema_version": "1.0",
            "label": "CLI Test",
            "platform": "gba",
            "payload_format": "gba"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(&cover, PNG).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_romx"))
        .args(["pack"])
        .arg(&rom)
        .arg(&metadata)
        .args(["--cover"])
        .arg(&cover)
        .args(["--output"])
        .arg(&packed)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let inspect = romx(&[&packed], &["inspect"]);
    assert!(inspect.status.success());
    let info: Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(info["metadata"]["label"], "CLI Test");
    assert_eq!(info["has_cover"], true);

    let verify = romx(&[&packed], &["verify"]);
    assert!(verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stdout).contains("valid ROMX"));

    let invalid = root.path().join("invalid.gbax");
    fs::write(&invalid, b"not a ROMX container").unwrap();
    let rejected = romx(&[&invalid], &["verify"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("error:"));

    let extract = Command::new(env!("CARGO_BIN_EXE_romx"))
        .arg("extract")
        .arg(&packed)
        .arg("--output")
        .arg(&extracted)
        .output()
        .unwrap();
    assert!(extract.status.success());
    assert_eq!(
        fs::read(extracted.join("payload.gba")).unwrap(),
        b"rom-bytes"
    );
    assert_eq!(fs::read(extracted.join("cover.png")).unwrap(), PNG);
}

#[test]
fn import_and_export_lpl_commands_work() {
    let root = tempdir().unwrap();
    let source = root.path().join("source");
    let thumbnails = root.path().join("thumbnails");
    let lpl = root.path().join("00-GB.lpl");
    let romx_dir = root.path().join("romx");
    let exported = root.path().join("exported");
    fs::create_dir_all(source.join("roms/00-GB")).unwrap();
    fs::create_dir_all(thumbnails.join("00-GB/Named_Snaps")).unwrap();
    fs::write(source.join("roms/00-GB/1.gb"), b"gb-rom").unwrap();
    fs::write(thumbnails.join("00-GB/Named_Snaps/1.png"), PNG).unwrap();
    fs::write(
        &lpl,
        serde_json::to_vec(&json!({
            "version": "1.5",
            "items": [{"path": "/roms/00-GB/1.gb", "label": "Game One"}]
        }))
        .unwrap(),
    )
    .unwrap();

    let import = Command::new(env!("CARGO_BIN_EXE_romx"))
        .arg("import-lpl")
        .arg(&lpl)
        .arg("--rom-root")
        .arg(&source)
        .arg("--cover-root")
        .arg(&thumbnails)
        .arg("--output")
        .arg(&romx_dir)
        .output()
        .unwrap();
    assert!(
        import.status.success(),
        "{}",
        String::from_utf8_lossy(&import.stderr)
    );
    assert!(romx_dir.join("000001.gbx").is_file());

    let export = Command::new(env!("CARGO_BIN_EXE_romx"))
        .arg("export-lpl")
        .arg(&romx_dir)
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    assert_eq!(
        fs::read(exported.join("roms/00-GB/000001.gb")).unwrap(),
        b"gb-rom"
    );
    assert!(exported
        .join("thumbnails/00-GB/Named_Snaps/Game One.png")
        .is_file());
    assert!(exported.join("playlists/00-GB.lpl").is_file());
}
