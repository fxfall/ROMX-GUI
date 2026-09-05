use image::{DynamicImage, GenericImageView, ImageBuffer, ImageFormat, Rgb};
use romx_core::{
    classify_gb_payload, crc32, launch_format_id_for_extension, normalize_cover_bytes, pack_bytes,
    pack_bytes_with_crc32, pack_bytes_with_options, pack_bytes_with_writer_options,
    pack_entries_to_path_with_writer_options, platform_id_for_name, read_bytes,
    read_metadata_cover_bytes, read_metadata_cover_path, read_path, recommended_mutable_capacity,
    recommended_mutable_capacity_for_save_bytes, recommended_mutable_capacity_for_save_count,
    recommended_mutable_entry_capacity, MutableSaveBundle, MutableSaveFile, PackEntry, PackOptions,
    DEFAULT_SAVE_OBJECT_CAPACITY, FLAG_COVER, FLAG_METADATA,
    RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY, RECOMMENDED_DISC_MUTABLE_CAPACITY,
    RECOMMENDED_NDS_MUTABLE_CAPACITY,
};
use serde_json::json;
use std::io::Cursor;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x00, 0x02, 0x00,
    0x00, 0x05, 0x00, 0x01, 0x7a, 0x5e, 0xab, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

#[test]
fn roundtrip_preserves_rom_metadata_and_png() {
    let rom = b"example-rom";
    let metadata = json!({
        "schema_version": "0.2.0",
        "name": "Example",
    });
    let bytes = pack_bytes(rom, Some(&metadata), Some(PNG)).unwrap();
    let document = read_bytes(&bytes).unwrap();
    assert_eq!(document.rom, rom);
    assert_eq!(document.metadata.as_ref().unwrap()["name"], "Example");
    assert_eq!(document.metadata.as_ref().unwrap()["crc32"], crc32(rom));
    assert_eq!(document.cover.as_ref().unwrap(), PNG);
    assert_eq!(
        document.footer.flags & (FLAG_METADATA | FLAG_COVER),
        FLAG_METADATA | FLAG_COVER
    );
}

#[test]
fn temporary_writer_computes_metadata_crc_in_the_payload_stream() {
    let metadata = json!({
        "schema_version": "0.2.0",
        "name": "Temporary output",
    });
    let options = PackOptions {
        include_entry_crc32: false,
        output_is_temporary: true,
        post_write_validation: false,
        ..Default::default()
    };
    let bytes =
        pack_bytes_with_writer_options(b"temporary-payload", Some(&metadata), None, &options)
            .unwrap();
    let document = read_bytes(&bytes).unwrap();
    assert_eq!(
        document.metadata.as_ref().unwrap()["crc32"],
        crc32(b"temporary-payload")
    );
}

#[test]
fn mutable_capacity_uses_romx_guidance_and_roundtrips() {
    assert_eq!(
        recommended_mutable_capacity("gba", "gba"),
        RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY
    );
    assert_eq!(
        recommended_mutable_capacity("gba", "iso"),
        RECOMMENDED_DISC_MUTABLE_CAPACITY
    );
    assert_eq!(
        recommended_mutable_capacity("nds", "nds"),
        RECOMMENDED_NDS_MUTABLE_CAPACITY
    );

    let options = PackOptions {
        mutable_capacity: RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY,
        ..Default::default()
    };
    let bytes = pack_bytes_with_writer_options(b"mutable-rom", None, None, &options).unwrap();
    let document = read_bytes(&bytes).unwrap();
    assert_eq!(
        document.footer.mutable_capacity,
        RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY
    );
}

#[test]
fn mutable_capacity_uses_save_bytes_and_directory_header() {
    assert_eq!(recommended_mutable_entry_capacity(0), 8);
    assert_eq!(recommended_mutable_entry_capacity(1), 8);
    assert_eq!(recommended_mutable_entry_capacity(7), 16);
    assert_eq!(recommended_mutable_entry_capacity(14), 16);

    let save_bytes = DEFAULT_SAVE_OBJECT_CAPACITY * 9;
    let capacity = recommended_mutable_capacity_for_save_bytes(
        RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY,
        9,
        save_bytes,
    );
    assert!(capacity >= save_bytes);
    assert_eq!(capacity % 4096, 0);
    assert_eq!(
        recommended_mutable_capacity_for_save_count(RECOMMENDED_NDS_MUTABLE_CAPACITY, 7),
        RECOMMENDED_NDS_MUTABLE_CAPACITY
    );

    let options = PackOptions {
        mutable_capacity: capacity,
        mutable_entry_capacity: recommended_mutable_entry_capacity(9),
        ..Default::default()
    };
    let bytes = pack_bytes_with_writer_options(b"multi-save-rom", None, None, &options).unwrap();
    read_bytes(&bytes).unwrap();
}

#[test]
fn mutable_save_bundle_is_written_as_a_readable_romx_object() {
    let root = tempfile::tempdir().unwrap();
    let save = root.path().join("中文存档.sav");
    std::fs::write(&save, vec![0x5au8; 512 * 1024]).unwrap();
    let options = PackOptions {
        mutable_capacity: RECOMMENDED_NDS_MUTABLE_CAPACITY,
        mutable_entry_capacity: 8,
        mutable_save_bundles: vec![MutableSaveBundle {
            key: "中文存档".into(),
            files: vec![MutableSaveFile {
                path: "中文存档.sav".into(),
                source: save,
            }],
        }],
        ..Default::default()
    };
    let bytes = pack_bytes_with_writer_options(b"nds-payload", None, None, &options).unwrap();
    let romx_path = root.path().join("bundle.romx");
    std::fs::write(&romx_path, &bytes).unwrap();
    let info = romx_core::read_mutable_save_objects(&romx_path).unwrap();
    assert_eq!(info.objects.len(), 1);
    assert_eq!(info.objects[0].key, "中文存档");
    assert!(info.objects[0].data_capacity > info.objects[0].data_size);
    assert_eq!(info.objects[0].files.len(), 1);
    assert_eq!(info.objects[0].files[0].path, "中文存档.sav");
    assert_eq!(read_bytes(&bytes).unwrap().rom, b"nds-payload");
}

#[test]
fn multi_entry_writer_preserves_descriptor_paths_and_entrypoint_offset() {
    let root = tempfile::tempdir().unwrap();
    let cue = root.path().join("Game.cue");
    let track = root.path().join("Track 01.bin");
    std::fs::write(&cue, b"FILE \"Track 01.bin\" BINARY\n").unwrap();
    std::fs::write(&track, vec![0x5au8; 4096]).unwrap();
    let metadata_path = root.path().join("metadata.json");
    std::fs::write(
        &metadata_path,
        br#"{"schema_version":"0.2.0","name":"Multi CUE"}"#,
    )
    .unwrap();
    let output = root.path().join("game.romx");
    let entries = vec![
        PackEntry {
            path: "Game.cue".into(),
            source: cue,
            format_id: 0,
            entrypoint: true,
        },
        PackEntry {
            path: "Track 01.bin".into(),
            source: track,
            format_id: 0,
            entrypoint: false,
        },
    ];
    let options = PackOptions {
        platform_id: platform_id_for_name("saturn"),
        launch_format_id: launch_format_id_for_extension("cue"),
        ..Default::default()
    };
    pack_entries_to_path_with_writer_options(
        &entries,
        Some("Game.cue"),
        Some(&metadata_path),
        None,
        &output,
        &options,
    )
    .unwrap();
    let document = read_path(&output).unwrap();
    assert_eq!(document.entries.len(), 2);
    assert_eq!(document.entries[0].path, "Game.cue");
    assert!(document.entries[0].entrypoint);
    assert_eq!(document.entries[0].data_offset, 0);
    assert_eq!(document.entries[1].path, "Track 01.bin");
    assert_eq!(
        document.entries[1].data_offset,
        document.entries[0].data_size
    );
    assert_eq!(document.footer.platform_id, platform_id_for_name("saturn"));
    assert_eq!(
        document.footer.launch_format_id,
        launch_format_id_for_extension("cue")
    );
    assert_eq!(document.metadata.unwrap()["name"], "Multi CUE");
}

#[test]
fn custom_crc32_overrides_lookup_key_but_not_footer_integrity() {
    let rom = b"example-rom";
    let metadata = json!({
        "schema_version": "0.2.0",
        "name": "Example",
        "crc32": "deadbeef"
    });
    let bytes = pack_bytes_with_crc32(rom, Some(&metadata), None, Some("A1B2C3D4")).unwrap();
    let document = read_bytes(&bytes).unwrap();
    assert_eq!(document.metadata.as_ref().unwrap()["crc32"], "a1b2c3d4");
    assert_eq!(
        romx_core::payload_sha256(&document.rom),
        romx_core::payload_sha256(rom)
    );
}

#[test]
fn preview_reader_loads_only_footer_metadata_and_cover() {
    let metadata = json!({
        "schema_version": "0.2.0",
        "name": "Preview",
    });
    let bytes = pack_bytes(&vec![7u8; 4096], Some(&metadata), Some(PNG)).unwrap();
    let preview = read_metadata_cover_bytes(&bytes).unwrap();
    assert_eq!(preview.footer.rom.size, 4096);
    assert_eq!(preview.metadata.as_ref().unwrap()["name"], "Preview");
    assert_eq!(preview.cover.as_deref(), Some(PNG));

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("preview.romx");
    std::fs::write(&path, bytes).unwrap();
    let from_path = read_metadata_cover_path(&path).unwrap();
    assert_eq!(from_path.footer, preview.footer);
    assert_eq!(from_path.metadata, preview.metadata);
    assert_eq!(from_path.cover, preview.cover);
}

#[test]
fn cover_png_is_preserved_without_target_and_other_formats_are_png_converted() {
    let source = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(2, 1, Rgb([255, 0, 0])));
    let mut jpeg = Cursor::new(Vec::new());
    source.write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
    for format in [
        ImageFormat::Jpeg,
        ImageFormat::WebP,
        ImageFormat::Gif,
        ImageFormat::Bmp,
    ] {
        let mut encoded = Cursor::new(Vec::new());
        source.write_to(&mut encoded, format).unwrap();
        let converted = normalize_cover_bytes(encoded.get_ref(), None).unwrap();
        assert!(converted.starts_with(romx_core::PNG_SIGNATURE));
        assert_eq!(
            image::load_from_memory(&converted).unwrap().dimensions(),
            (2, 1)
        );
    }

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
        "schema_version": "0.2.0",
        "name": "Cover metadata",
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
    assert!(cover_metadata.get("sha256").is_none());
}

#[test]
fn rejects_overlapping_regions() {
    let metadata = json!({"schema_version":"0.2.0","name":"x"});
    let mut bytes = pack_bytes(b"rom", Some(&metadata), None).unwrap();
    let footer_start = bytes.len() - 128;
    bytes[footer_start + 0x08..footer_start + 0x10].copy_from_slice(&0u64.to_le_bytes());
    assert!(read_bytes(&bytes).is_err());
}

#[test]
fn allows_cover_without_metadata() {
    let bytes = pack_bytes(b"rom", None, Some(PNG)).unwrap();
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
