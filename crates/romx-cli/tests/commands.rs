use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x00, 0x02, 0x00,
    0x00, 0x05, 0x00, 0x01, 0x7a, 0x5e, 0xab, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

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
    let packed = root.path().join("game.romx");
    let extracted = root.path().join("extracted");
    fs::write(&rom, b"rom-bytes").unwrap();
    fs::write(
        &metadata,
        serde_json::to_vec(&json!({
            "schema_version": "0.2.0",
            "name": "CLI Test",
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
    assert_eq!(info["metadata"]["name"], "CLI Test");
    assert_eq!(info["has_cover"], true);

    let verify = romx(&[&packed], &["verify"]);
    assert!(verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stdout).contains("valid ROMX"));

    let invalid = root.path().join("invalid.romx");
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
    assert_eq!(fs::read(extracted.join("game.gba")).unwrap(), b"rom-bytes");
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
    assert!(romx_dir.join("1.gb.romx").is_file());

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
        fs::read(exported.join("roms/00-GB/1.gb")).unwrap(),
        b"gb-rom"
    );
    assert!(exported
        .join("thumbnails/00-GB/Named_Snaps/1.png")
        .is_file());
    assert!(exported.join("playlists/00-GB.lpl").is_file());
}

#[test]
fn pack_set_command_preserves_multiple_entries() {
    let root = tempdir().unwrap();
    let cue = root.path().join("游戏.cue");
    let track = root.path().join("轨道.bin");
    let packed = root.path().join("游戏.cue.romx");
    fs::write(&cue, b"FILE \"track.bin\" BINARY\n  TRACK 01 MODE1/2352\n").unwrap();
    fs::write(&track, b"disc-track-bytes").unwrap();

    let assignment = format!("disc.cue={}", cue.display());
    let sidecar = format!("track.bin={}", track.display());
    let output = Command::new(env!("CARGO_BIN_EXE_romx"))
        .args([
            "pack-set",
            "--entry",
            &assignment,
            "--entry",
            &sidecar,
            "--entrypoint",
            "disc.cue",
            "--platform",
            "playstation",
            "--launch-format",
            "cue",
            "--output",
        ])
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
    assert_eq!(info["entries"].as_array().unwrap().len(), 2);
    assert_eq!(info["entries"][0]["path"], Value::String("disc.cue".into()));
    assert_eq!(info["entries"][0]["entrypoint"], true);
}
