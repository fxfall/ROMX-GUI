use romx_core::{
    pack_bytes, pack_bytes_with_writer_options, pack_to_path, validate_bytes, validate_png_bytes,
    PackOptions, ValidationStatus,
};
use serde_json::json;
use std::fs;
use tempfile::tempdir;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x00, 0x02, 0x00,
    0x00, 0x05, 0x00, 0x01, 0x7a, 0x5e, 0xab, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

#[test]
fn writer_uses_the_frozen_v020_contract() {
    let metadata = json!({"schema_version":"0.2.0", "name":"Fixture", "origin_crc32":"00000000", "description":"compacted"});
    let bytes = pack_bytes(b"abc", Some(&metadata), Some(PNG)).unwrap();
    let document = romx_core::read_bytes(&bytes).unwrap();
    assert_eq!(document.footer.version, 2);
    assert_eq!(
        document.metadata.as_ref().unwrap()["schema_version"],
        "0.2.0"
    );
    assert_eq!(document.metadata.as_ref().unwrap()["crc32"], "352441c2");
    assert_eq!(
        document.metadata.as_ref().unwrap()["origin_crc32"],
        "00000000"
    );
    assert_eq!(document.footer.reserved, [0; 44]);
    assert_eq!(validate_png_bytes(PNG).unwrap().width, 1);
    assert!(String::from_utf8(bytes).is_err());
}

#[test]
fn immutable_sha256_is_opt_in_and_validated() {
    let metadata = json!({"schema_version":"0.2.0", "name":"Fixture"});
    let bytes = pack_bytes_with_writer_options(
        b"abc",
        Some(&metadata),
        None,
        &PackOptions {
            body_sha256: true,
            ..Default::default()
        },
    )
    .unwrap();
    let report = validate_bytes(&bytes).unwrap();
    assert_eq!(report.structure, ValidationStatus::Valid);
    assert_eq!(report.body_sha256, ValidationStatus::Valid);
}

#[test]
fn strict_metadata_and_duplicate_keys_are_rejected() {
    let root = tempdir().unwrap();
    let rom = root.path().join("game.gb");
    let metadata = root.path().join("metadata.json");
    let output = root.path().join("game.romx");
    fs::write(&rom, b"abc").unwrap();
    fs::write(
        &metadata,
        br#"{"schema_version":"0.2.0","name":"A","name":"B"}"#,
    )
    .unwrap();
    assert!(pack_to_path(&rom, Some(&metadata), None, &output).is_err());
    assert!(!output.exists());
    assert!(pack_bytes(
        b"abc",
        Some(&json!({"schema_version":"0.1.0", "name":"old"})),
        None
    )
    .is_err());
    assert!(pack_bytes(
        b"abc",
        Some(&json!({"schema_version":"0.2.0", "name":"x", "platform":"gba"})),
        None
    )
    .is_err());
}
