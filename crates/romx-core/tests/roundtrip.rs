use romx_core::{classify_gb_payload, pack_bytes, read_bytes, FLAG_BODY_SHA256, FLAG_COVER, FLAG_METADATA};
use serde_json::json;

#[test]
fn roundtrip_preserves_rom_metadata_and_png() {
    let rom = b"example-rom";
    let metadata = json!({
        "schema_version": "1.0",
        "label": "Example",
        "platform": "gba",
        "payload_format": "gba"
    });
    let cover = b"\x89PNG\r\n\x1a\nminimal";
    let bytes = pack_bytes(rom, Some(&metadata), Some(cover)).unwrap();
    let document = read_bytes(&bytes).unwrap();
    assert_eq!(document.rom, rom);
    assert_eq!(document.metadata.as_ref().unwrap()["label"], "Example");
    assert_eq!(document.cover.as_ref().unwrap(), cover);
    assert_eq!(
        document.footer.flags & (FLAG_METADATA | FLAG_COVER | FLAG_BODY_SHA256),
        FLAG_METADATA | FLAG_COVER | FLAG_BODY_SHA256
    );
}

#[test]
fn rejects_overlapping_regions() {
    let metadata =
        json!({"schema_version":"1.0","label":"x","platform":"gba","payload_format":"gba"});
    let mut bytes = pack_bytes(b"rom", Some(&metadata), None).unwrap();
    let footer_start = bytes.len() - 128;
    bytes[footer_start + 0x18..footer_start + 0x20].copy_from_slice(&0u64.to_le_bytes());
    assert!(read_bytes(&bytes).is_err());
}

#[test]
fn allows_cover_without_metadata() {
    let bytes = pack_bytes(b"rom", None, Some(b"\x89PNG\r\n\x1a\ncover")).unwrap();
    let document = read_bytes(&bytes).unwrap();
    assert!(document.metadata.is_none());
    assert!(document.cover.is_some());
}

#[test]
fn cgb_flag_policy_is_explicit() {
    let mut rom = vec![0u8; 0x144];
    rom[0x143] = 0xC0;
    assert_eq!(classify_gb_payload(&rom, Some("gb" )).unwrap(), "gbc");
    rom[0x143] = 0x80;
    assert_eq!(classify_gb_payload(&rom, Some("gb")).unwrap(), "gb");
    assert_eq!(classify_gb_payload(&rom, Some("gbc")).unwrap(), "gbc");
    assert!(classify_gb_payload(&rom, None).is_err());
}
