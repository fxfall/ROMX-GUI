use romx_core::{
    pack_bytes, pack_bytes_with_writer_options, pack_to_path, payload_sha256, read_bytes,
    validate_bytes, validate_png_bytes, Crc32Status, Footer, PackOptions, Region, ValidationStatus,
    FLAG_BODY_SHA256, FLAG_COVER, FLAG_METADATA,
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
fn writer_uses_the_frozen_metadata_and_footer_contract() {
    let metadata = json!({
        "schema_version": "1.0",
        "name": "Fixture",
        "platform": "gba",
        "payload_format": "gba",
        "origin_crc32": "00000000",
        "description": "  compacted  "
    });
    let bytes = pack_bytes(b"abc", Some(&metadata), Some(PNG)).unwrap();
    let document = read_bytes(&bytes).unwrap();
    assert_eq!(document.metadata.as_ref().unwrap()["name"], "Fixture");
    assert_eq!(document.metadata.as_ref().unwrap()["crc32"], "352441c2");
    assert_eq!(
        document.metadata.as_ref().unwrap()["origin_crc32"],
        "352441c2"
    );
    assert!(document.metadata.as_ref().unwrap().get("label").is_none());
    assert_eq!(document.footer.reserved, [0; 32]);
    assert_eq!(document.footer.flags, FLAG_METADATA | FLAG_COVER);
    assert_eq!(document.footer.body_sha256, [0; 32]);
    assert_eq!(validate_png_bytes(PNG).unwrap().width, 1);
    assert!(String::from_utf8(bytes).is_err());
}

#[test]
fn body_hash_is_opt_in_and_optional_regions_can_be_salvaged() {
    let metadata = json!({
        "schema_version": "1.0",
        "name": "Fixture",
        "platform": "gb",
        "payload_format": "gb"
    });
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
    let document = read_bytes(&bytes).unwrap();
    assert_eq!(document.footer.flags, FLAG_METADATA | FLAG_BODY_SHA256);

    let invalid_metadata = b"\xef\xbb\xbf{}";
    let invalid_cover = b"not-a-png";
    let mut body = Vec::new();
    body.extend_from_slice(b"abc");
    body.extend_from_slice(invalid_metadata);
    body.extend_from_slice(invalid_cover);
    let footer = Footer {
        version: 1,
        rom: Region { offset: 0, size: 3 },
        metadata: Region {
            offset: 3,
            size: invalid_metadata.len() as u64,
        },
        cover: Region {
            offset: 3 + invalid_metadata.len() as u64,
            size: invalid_cover.len() as u64,
        },
        reserved: [0; 32],
        flags: FLAG_METADATA | FLAG_COVER | FLAG_BODY_SHA256,
        body_sha256: payload_sha256(&body),
    };
    body.extend_from_slice(&footer.encode());
    let salvaged = read_bytes(&body).unwrap();
    assert_eq!(salvaged.rom, b"abc");
    assert!(salvaged.metadata.is_none());
    assert!(salvaged.cover.is_none());
    let report = validate_bytes(&body).unwrap();
    assert_eq!(report.metadata, ValidationStatus::Invalid);
    assert_eq!(report.cover, ValidationStatus::Invalid);
    assert_eq!(report.metadata_crc32, Crc32Status::Invalid);
}

#[test]
fn duplicate_metadata_keys_are_rejected_by_path_writer() {
    let root = tempdir().unwrap();
    let rom = root.path().join("game.gb");
    let metadata = root.path().join("metadata.json");
    let output = root.path().join("game.gbx");
    fs::write(&rom, b"abc").unwrap();
    fs::write(
        &metadata,
        br#"{"schema_version":"1.0","name":"A","name":"B","platform":"gb","payload_format":"gb"}"#,
    )
    .unwrap();
    assert!(pack_to_path(&rom, Some(&metadata), None, &output).is_err());
    assert!(!output.exists());
}
