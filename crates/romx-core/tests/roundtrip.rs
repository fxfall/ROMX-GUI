use image::{DynamicImage, GenericImageView, ImageBuffer, ImageFormat, Rgb};
use romx_core::{
    classify_gb_payload, crc32, normalize_cover_bytes, pack_bytes, pack_bytes_with_crc32,
    pack_bytes_with_options, read_bytes, FLAG_BODY_SHA256, FLAG_COVER, FLAG_METADATA,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::Cursor;

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
    assert_eq!(document.metadata.as_ref().unwrap()["crc32"], crc32(rom));
    assert_eq!(document.cover.as_ref().unwrap(), cover);
    assert_eq!(
        document.footer.flags & (FLAG_METADATA | FLAG_COVER | FLAG_BODY_SHA256),
        FLAG_METADATA | FLAG_COVER | FLAG_BODY_SHA256
    );
}

#[test]
fn custom_crc32_overrides_lookup_key_but_not_footer_integrity() {
    let rom = b"example-rom";
    let metadata = json!({
        "schema_version": "1.0",
        "label": "Example",
        "platform": "gba",
        "payload_format": "gba",
        "crc32": "deadbeef"
    });
    let bytes = pack_bytes_with_crc32(rom, Some(&metadata), None, Some("A1B2C3D4")).unwrap();
    let document = read_bytes(&bytes).unwrap();
    assert_eq!(document.metadata.as_ref().unwrap()["crc32"], "a1b2c3d4");
    let expected_hash: [u8; 32] = Sha256::digest(rom).into();
    assert_eq!(document.footer.rom_sha256, expected_hash);
}

#[test]
fn cover_png_is_preserved_without_target_and_other_formats_are_png_converted() {
    let source = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(2, 1, Rgb([255, 0, 0])));
    let mut jpeg = Cursor::new(Vec::new());
    source.write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
    let converted = normalize_cover_bytes(jpeg.get_ref(), None).unwrap();
    assert!(converted.starts_with(romx_core::PNG_SIGNATURE));
    assert_eq!(
        image::load_from_memory(&converted).unwrap().dimensions(),
        (2, 1)
    );

    let mut png = Cursor::new(Vec::new());
    source.write_to(&mut png, ImageFormat::Png).unwrap();
    let original = png.into_inner();
    assert_eq!(normalize_cover_bytes(&original, None).unwrap(), original);
    let resized = normalize_cover_bytes(&original, Some((8, 6))).unwrap();
    assert_eq!(
        image::load_from_memory(&resized).unwrap().dimensions(),
        (8, 6)
    );

    let metadata = json!({
        "schema_version": "1.0",
        "label": "Cover metadata",
        "platform": "gba",
        "payload_format": "gba"
    });
    let packed = pack_bytes_with_options(
        b"rom",
        Some(&metadata),
        Some(jpeg.get_ref()),
        None,
        Some((8, 6)),
    )
    .unwrap();
    let document = read_bytes(&packed).unwrap();
    let cover_metadata = &document.metadata.as_ref().unwrap()["cover"];
    assert_eq!(cover_metadata["mime_type"], "image/png");
    assert_eq!(cover_metadata["width"], 8);
    assert_eq!(cover_metadata["height"], 6);
    assert_eq!(cover_metadata["sha256"].as_str().unwrap().len(), 64);
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
    assert_eq!(classify_gb_payload(&rom, Some("gb")).unwrap(), "gbc");
    rom[0x143] = 0x80;
    assert_eq!(classify_gb_payload(&rom, Some("gb")).unwrap(), "gb");
    assert_eq!(classify_gb_payload(&rom, Some("gbc")).unwrap(), "gbc");
    assert!(classify_gb_payload(&rom, None).is_err());
}
